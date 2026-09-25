<script setup lang="ts">
// =============================================================================
// InstanceMonitor.vue —— 实例监控（可复用组件，vLLM 推理实例级监控）
//
// 从 LlmModels.vue「实例监控」Tab 原样迁出的 UI + 逻辑，抽成独立组件供两处复用：
//   1. LlmModels.vue「实例监控」Tab（原位替换，功能不变）
//   2. ApiGateway.vue「实例监控」Tab（网关运营者看本机哪些实例在跑、健康状态）
//
// 三块能力：
//   - 实例选择 + vLLM /metrics 拉取展示（10s 自动轮询，document.hidden 暂停）
//   - 网关可路由模型聚合（GET /api/v1/llm/gateway/models，真实探测各 running
//     实例的 /v1/models；gateway_visible / unreachable 两组，30s 低频自动刷新）
//   - 状态语义与 LlmModels 原版一致：离线（reachable:false）不是错误，勿走红箱
//
// Props：
//   - filterInstanceId?: string —— 只看某实例（锁定、隐藏实例下拉；外部已知
//     实例 id 的嵌入场景用）
//   - compact?: boolean —— 紧凑模式（指标卡网格更密、隐藏说明行；嵌入窄容器用）
//
// 数据自包含：实例列表组件内自行拉取（GET /api/v1/llm/instances，公开读），
// 宿主页面零接线——挂载即用。
// =============================================================================
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { endpoints, get } from '@/api/client';

// =============================================================================
// Props
// =============================================================================
const props = defineProps<{
  /** 只看某实例：锁定该实例、隐藏实例下拉。 */
  filterInstanceId?: string;
  /** 紧凑模式：网格更密 + 隐藏说明行。 */
  compact?: boolean;
}>();

/** 锁定模式 = 外部指定了实例 id（隐藏下拉，忽略自选）。 */
const locked = computed(() => !!props.filterInstanceId?.trim());

// =============================================================================
// 数据模型
// =============================================================================
/** 实例（组件只消费 id/name/model/port/status 五个字段）。 */
interface MonInstance {
  id?: string;
  name?: string;
  model?: string;
  port?: number;
  status?: string;
  [k: string]: unknown;
}
/** 单次指标快照（GET /metrics 元素；所有字段可空——速率三字段首次采样为 null）。 */
interface MetricsSnapshot {
  /** 运行中请求数（Gauge）。 */
  num_requests_running?: number | null;
  /** 排队请求数（Gauge）。 */
  num_requests_waiting?: number | null;
  /** KV cache 占用率 0-1（Gauge）。 */
  gpu_cache_usage?: number | null;
  /** prefix cache 命中率 0-1（Gauge）。 */
  prefix_cache_hit_rate?: number | null;
  /** 生成 token 速率（tok/s；首次采样无历史差值为 null）。 */
  generation_tokens_per_sec?: number | null;
  /** prompt token 速率（tok/s；首次采样为 null）。 */
  prompt_tokens_per_sec?: number | null;
  /** 完成请求速率（req/s；首次采样为 null）。 */
  requests_success_per_sec?: number | null;
  /** 端到端请求时延均值（毫秒）。 */
  e2e_latency_ms?: number | null;
}
/** GET /api/v1/llm/instances/:id/metrics 响应（离线=reachable:false 且 metrics:null）。 */
interface InstanceMetricsResponse {
  instance_id?: string;
  /** 真实 vLLM /metrics 是否抓取成功。 */
  reachable?: boolean;
  /** 是否合成模拟数据（NEXOS_LLM_METRICS_SIMULATE 且真实端口不通时 true）。 */
  simulated?: boolean;
  /** 采集时刻（ISO 8601）。 */
  collected_at?: string;
  /** 抓取目标（http://127.0.0.1:<port>）。 */
  base_url?: string;
  metrics?: MetricsSnapshot | null;
}
/** 网关可见的一条实例（/v1/models 探测成功；handlers/llm.rs GatewayModelEntry）。 */
interface GatewayModelEntry {
  instance_id?: string;
  name?: string;
  served_model_name?: string | null;
  port?: number;
  alive?: boolean;
  /** vLLM /v1/models 返回的原始模型对象（data[] 原样）。 */
  models?: unknown[];
  /** 解析出的 data[].id 列表（网关路由/计费的真实键）。 */
  model_ids?: string[];
}
/** status=running 但 /v1/models 探测失败的实例（GatewayUnreachableEntry）。 */
interface GatewayUnreachableEntry {
  instance_id?: string;
  name?: string;
  port?: number;
  reason?: string;
}
/** GET /api/v1/llm/gateway/models 响应（网关聚合视图）。 */
interface GatewayModelsResponse {
  gateway_visible?: GatewayModelEntry[];
  unreachable?: GatewayUnreachableEntry[];
}

// =============================================================================
// 实例列表（自包含：挂载即拉，宿主零接线）
// =============================================================================
const instances = ref<MonInstance[]>([]);
const instancesLoading = ref(false);
/** 实例列表拉取失败提示（公开 GET，失败不阻塞 metrics 手动选择）。 */
const instancesError = ref('');

async function loadInstances(): Promise<void> {
  instancesLoading.value = true;
  instancesError.value = '';
  try {
    const raw = await endpoints.llmInstances();
    instances.value = Array.isArray(raw) ? (raw as MonInstance[]) : [];
  } catch (e) {
    instancesError.value = friendlyError(e);
  } finally {
    instancesLoading.value = false;
  }
}

// =============================================================================
// 实例级 metrics（逻辑自 LlmModels.vue 原样迁出）
// =============================================================================
/** 轮询间隔（毫秒）。 */
const MT_POLL_MS = 10_000;

/** 监控实例 id（下拉 value；离线实例也可选，选后显示离线徽章）。 */
const mtSelectedId = ref<string>('');
/** 最近一次 metrics 响应（null=尚未采集）。 */
const mtData = ref<InstanceMetricsResponse | null>(null);
const mtLoading = ref(false);
const mtError = ref('');
/** 轮询定时器句柄（null=未在轮询）。 */
let mtTimer: ReturnType<typeof setInterval> | null = null;
/** 请求序号：切换实例后丢弃迟到的旧响应。 */
let mtReqSeq = 0;

/** 可监控实例 = 全部实例（离线实例也能看离线状态）。 */
const mtInstances = computed(() => instances.value);

/** 选中实例对象（计算）。 */
const mtSelected = computed<MonInstance | undefined>(() =>
  instances.value.find((i) => String(i.id ?? '') === mtSelectedId.value),
);

/** 实例列表就绪后默认选第一个 running（无 running 选第一个）；锁定模式除外。 */
watch(
  instances,
  () => {
    if (locked.value || mtSelectedId.value) return;
    const first =
      instances.value.find((i) => i.status === 'running') ?? instances.value[0];
    if (first) mtSelectedId.value = String(first.id ?? '');
  },
  { immediate: true },
);

/** 状态徽章：在线绿 / 离线灰 / 模拟橙。 */
const mtStatus = computed<{ cls: string; label: string }>(() => {
  const d = mtData.value;
  if (!d) return { cls: 'pill-muted', label: '未知' };
  if (d.simulated) return { cls: 'mt-pill-sim', label: '模拟' };
  if (d.reachable) return { cls: 'pill-ok', label: '在线' };
  return { cls: 'pill-muted', label: '离线' };
});

/** 离线 = reachable:false && simulated:false（此时 metrics 为 null）。 */
const mtIsOffline = computed(() => {
  const d = mtData.value;
  return !!d && !d.reachable && !d.simulated;
});

/** 是否模拟数据（simulated:true → 橙色横幅 + 「模拟」徽章）。 */
const mtIsSimulated = computed(() => !!mtData.value?.simulated);

/** 当前指标快照（无数据/离线时为空对象 → 各卡显示「—」）。 */
const mtSnap = computed<MetricsSnapshot>(() => mtData.value?.metrics ?? {});

async function mtFetch(): Promise<void> {
  const id = mtSelectedId.value;
  if (!id) return;
  const seq = ++mtReqSeq;
  mtLoading.value = true;
  try {
    const raw = await endpoints.llmInstanceMetrics(id);
    if (seq !== mtReqSeq) return; // 迟到响应（已切换实例）丢弃
    mtData.value = raw as InstanceMetricsResponse;
    mtError.value = '';
  } catch (e) {
    if (seq !== mtReqSeq) return;
    mtData.value = null;
    mtError.value = friendlyError(e); // 404=实例不存在等真错误
  } finally {
    if (seq === mtReqSeq) mtLoading.value = false;
  }
}

function mtStopPolling(): void {
  if (mtTimer !== null) {
    clearInterval(mtTimer);
    mtTimer = null;
  }
}
function mtStartPolling(): void {
  mtStopPolling();
  mtTimer = setInterval(() => {
    if (document.hidden) return; // 页面隐藏时暂停轮询
    void mtFetch();
  }, MT_POLL_MS);
}

/** 页面重新可见：立即补一次采样（对齐隐藏期间暂停的节拍）。 */
function onVisibility(): void {
  if (document.hidden) return;
  if (mtSelectedId.value) void mtFetch();
  void gwFetch(); // 网关聚合同步补一轮（低频轮询隐藏期停拍）
}

// 切实例：清旧数据 + 立即刷新（重启 10s 节拍）
watch(mtSelectedId, () => {
  mtReqSeq++; // 使在途请求失效
  mtData.value = null;
  mtError.value = '';
  if (mtSelectedId.value) {
    void mtFetch();
    mtStartPolling();
  } else {
    mtStopPolling();
  }
});

// 锁定模式：外部指定实例 id → 直接锁定（外部变化跟随）
watch(
  () => props.filterInstanceId,
  (id) => {
    if (!id?.trim()) return;
    mtSelectedId.value = id.trim();
  },
  { immediate: true },
);

// =============================================================================
// 网关可路由模型聚合（GET /api/v1/llm/gateway/models，公开读）
// =============================================================================
/** 网关聚合刷新间隔（毫秒；探测有真实 HTTP 外呼，低于 metrics 的 10s）。 */
const GW_POLL_MS = 30_000;

const gwData = ref<GatewayModelsResponse | null>(null);
const gwLoading = ref(false);
const gwError = ref('');
let gwTimer: ReturnType<typeof setInterval> | null = null;
let gwReqSeq = 0;

/** 可见实例数 / 不可达数（面板摘要行）。 */
const gwVisibleCount = computed(() => gwData.value?.gateway_visible?.length ?? 0);
const gwUnreachableCount = computed(
  () => gwData.value?.unreachable?.length ?? 0,
);

async function gwFetch(): Promise<void> {
  const seq = ++gwReqSeq;
  gwLoading.value = true;
  try {
    const raw = await get<GatewayModelsResponse>('/api/v1/llm/gateway/models');
    if (seq !== gwReqSeq) return;
    gwData.value = raw;
    gwError.value = '';
  } catch (e) {
    if (seq !== gwReqSeq) return;
    gwError.value = friendlyError(e);
  } finally {
    if (seq === gwReqSeq) gwLoading.value = false;
  }
}

function gwStopPolling(): void {
  if (gwTimer !== null) {
    clearInterval(gwTimer);
    gwTimer = null;
  }
}
function gwStartPolling(): void {
  gwStopPolling();
  gwTimer = setInterval(() => {
    if (document.hidden) return;
    void gwFetch();
  }, GW_POLL_MS);
}

/** 手动刷新：metrics + 网关聚合一起刷。 */
function refreshAll(): void {
  void mtFetch();
  void gwFetch();
}

// =============================================================================
// 数值格式化（null → '—'；自 LlmModels.vue 迁出）
// =============================================================================
function friendlyError(e: unknown): string {
  const m = e instanceof Error ? e.message : String(e);
  return m;
}
function fmtMetricsInt(v?: number | null): string {
  return v == null ? '—' : String(Math.round(v));
}
/** 0-1 比率 → 百分比字符串（钳位 0-100）。 */
function fmtMetricsPct(v?: number | null): string {
  if (v == null) return '—';
  return `${fmtMetricsPctNum(v).toFixed(1)}%`;
}
/** 0-1 比率 → 0-100 数值（进度条宽度用）。 */
function fmtMetricsPctNum(v?: number | null): number {
  return v == null ? 0 : Math.max(0, Math.min(100, v * 100));
}
function fmtMetricsRate(v?: number | null): string {
  return v == null ? '—' : v.toFixed(1);
}
function fmtMetricsLatency(v?: number | null): string {
  return v == null ? '—' : `${v.toFixed(1)} ms`;
}
/** ISO 8601 → 本地可读时间（空/非法原样占位）。 */
function fmtDateTime(s?: string): string {
  if (!s) return '—';
  const d = new Date(s);
  return Number.isNaN(d.getTime()) ? s : d.toLocaleString();
}

// =============================================================================
// 生命周期
// =============================================================================
onMounted(() => {
  void loadInstances();
  void gwFetch();
  gwStartPolling();
  document.addEventListener('visibilitychange', onVisibility);
});

onBeforeUnmount(() => {
  mtStopPolling();
  gwStopPolling();
  document.removeEventListener('visibilitychange', onVisibility);
});
</script>

<template>
  <div class="im-root" :class="{ 'im-compact': compact }">
    <!-- 面板头：标题 + 说明 + 实例选择 + 刷新 -->
    <div class="panel-head">
      <span class="panel-title">实例监控</span>
      <span v-if="!compact" class="muted small">实例级 vLLM /metrics · 10s 自动轮询</span>
      <div v-if="!locked" class="head-actions">
        <select v-model="mtSelectedId" class="im-select" :disabled="mtLoading">
          <option value="" disabled>选择实例…</option>
          <option
            v-for="i in mtInstances"
            :key="String(i.id ?? '')"
            :value="String(i.id ?? '')"
          >
            {{ i.name || i.model }} · :{{ i.port }}
          </option>
        </select>
        <button class="btn btn-small" :disabled="mtLoading || !mtSelectedId" @click="refreshAll">
          <span class="spin" :class="{ spinning: mtLoading }" aria-hidden="true">↻</span>
          刷新
        </button>
      </div>
      <div v-else class="head-actions">
        <button class="btn btn-small" :disabled="mtLoading || !mtSelectedId" @click="refreshAll">
          <span class="spin" :class="{ spinning: mtLoading }" aria-hidden="true">↻</span>
          刷新
        </button>
      </div>
    </div>

    <div v-if="instancesError" class="error-box">实例列表拉取失败：{{ instancesError }}</div>

    <!-- 状态徽章 + 实例名 + base_url + 采集时间 -->
    <div class="card mt-info-card">
      <span class="pill" :class="mtStatus.cls">{{ mtStatus.label }}</span>
      <span class="mt-info-item">{{ mtSelected?.name || mtSelected?.model || '—' }}</span>
      <span class="mono muted small">{{ mtData?.base_url || '—' }}</span>
      <span class="muted small">采集于 {{ mtData?.collected_at ? fmtDateTime(mtData.collected_at) : '—' }}</span>
    </div>

    <!-- 模拟数据橙色横幅（simulated:true 必须醒目提示） -->
    <div v-if="mtIsSimulated" class="mt-banner-sim">
      当前为模拟数据（实例不可达，NEXOS_LLM_METRICS_SIMULATE）
    </div>

    <div v-if="mtError" class="error-box">监控拉取失败：{{ mtError }}</div>
    <div v-else-if="!mtSelectedId" class="card empty-card">暂无可监控实例（等待实例列表就绪）。</div>
    <div v-else-if="mtLoading && !mtData" class="card empty-card">采集中…</div>
    <!-- 离线（reachable:false && simulated:false && metrics:null；200 语义非错误） -->
    <div v-else-if="mtIsOffline" class="card empty-card">
      实例离线：vLLM /metrics 不可达。<br />
      请到实例管理启动实例（状态 running）后再刷新。
    </div>

    <!-- 指标卡网格 -->
    <section v-else class="mt-grid">
      <div class="card mt-card">
        <div class="stat-label">运行中请求</div>
        <div class="stat-value mt-num">{{ fmtMetricsInt(mtSnap.num_requests_running) }}</div>
        <div class="muted small">num_requests_running</div>
      </div>
      <div class="card mt-card">
        <div class="stat-label">排队请求</div>
        <div class="stat-value mt-num">{{ fmtMetricsInt(mtSnap.num_requests_waiting) }}</div>
        <div class="muted small">num_requests_waiting</div>
      </div>
      <div class="card mt-card">
        <div class="stat-label">KV cache 利用率</div>
        <div class="stat-value mt-num">{{ fmtMetricsPct(mtSnap.gpu_cache_usage) }}</div>
        <div class="prog-wrap">
          <span class="prog-bar"><span class="prog-fill fill-ok" :style="{ width: fmtMetricsPctNum(mtSnap.gpu_cache_usage) + '%' }" /></span>
          <span class="prog-text">{{ fmtMetricsPctNum(mtSnap.gpu_cache_usage).toFixed(0) }}%</span>
        </div>
      </div>
      <div class="card mt-card">
        <div class="stat-label">prefix 命中率</div>
        <div class="stat-value mt-num">{{ fmtMetricsPct(mtSnap.prefix_cache_hit_rate) }}</div>
        <div class="muted small">prefix_cache_hit_rate</div>
      </div>
      <div class="card mt-card">
        <div class="stat-label">生成 token 速率</div>
        <div class="stat-value mt-num">
          {{ fmtMetricsRate(mtSnap.generation_tokens_per_sec) }}<span class="mt-unit"> tok/s</span>
        </div>
        <div v-if="mtSnap.generation_tokens_per_sec == null" class="muted small">待二次采样</div>
      </div>
      <div class="card mt-card">
        <div class="stat-label">prompt token 速率</div>
        <div class="stat-value mt-num">
          {{ fmtMetricsRate(mtSnap.prompt_tokens_per_sec) }}<span class="mt-unit"> tok/s</span>
        </div>
        <div v-if="mtSnap.prompt_tokens_per_sec == null" class="muted small">待二次采样</div>
      </div>
      <div class="card mt-card">
        <div class="stat-label">请求完成速率</div>
        <div class="stat-value mt-num">
          {{ fmtMetricsRate(mtSnap.requests_success_per_sec) }}<span class="mt-unit"> req/s</span>
        </div>
        <div v-if="mtSnap.requests_success_per_sec == null" class="muted small">待二次采样</div>
      </div>
      <div class="card mt-card">
        <div class="stat-label">端到端延迟</div>
        <div class="stat-value mt-num">{{ fmtMetricsLatency(mtSnap.e2e_latency_ms) }}</div>
        <div class="muted small">e2e_request_latency</div>
      </div>
    </section>

    <!-- =================== 网关可路由模型（真实探测聚合） =================== -->
    <div class="card gw-card">
      <div class="panel-head">
        <span class="panel-title">网关可路由模型（真实探测 /v1/models）</span>
        <span v-if="!compact" class="muted small">
          可见 {{ gwVisibleCount }} · 不可达 {{ gwUnreachableCount }} · 30s 自动刷新
        </span>
        <div class="head-actions">
          <button class="btn btn-small" :disabled="gwLoading" @click="gwFetch">
            <span class="spin" :class="{ spinning: gwLoading }" aria-hidden="true">↻</span>
            刷新
          </button>
        </div>
      </div>

      <div v-if="gwError" class="error-box">网关聚合拉取失败：{{ gwError }}</div>
      <div v-else-if="gwLoading && !gwData" class="empty-inline muted small">探测中…</div>
      <template v-else-if="gwData">
        <!-- 可见组：实例名 :端口 + served_model_name + 模型 id chips -->
        <div v-if="!gwData.gateway_visible?.length" class="empty-inline muted small">
          当前没有任何实例可路由（无 running 实例或全部不可达）。
        </div>
        <div v-for="v in gwData.gateway_visible" :key="v.instance_id" class="gw-visible-row">
          <span class="pill pill-ok">可路由</span>
          <span class="gw-inst-name">{{ v.name || v.instance_id }}</span>
          <span class="mono muted small">:{{ v.port }}</span>
          <span v-if="v.served_model_name" class="pill pill-blue">{{ v.served_model_name }}</span>
          <span class="gw-model-chips">
            <span v-for="mid in v.model_ids" :key="mid" class="pill pill-cyan mono">{{ mid }}</span>
            <span v-if="!v.model_ids?.length" class="muted small">（vLLM 未返回模型 id）</span>
          </span>
        </div>
        <!-- 不可达组：配置声称 running 但 /v1/models 不通 -->
        <template v-if="gwData.unreachable?.length">
          <div class="gw-divider" />
          <div v-for="u in gwData.unreachable" :key="u.instance_id" class="gw-visible-row">
            <span class="pill pill-err">不可达</span>
            <span class="gw-inst-name">{{ u.name || u.instance_id }}</span>
            <span class="mono muted small">:{{ u.port }}</span>
            <span class="muted small gw-reason">{{ u.reason }}</span>
          </div>
        </template>
      </template>
    </div>
  </div>
</template>

<style scoped>
.im-root { display: flex; flex-direction: column; gap: 14px; }
.im-root.im-compact { gap: 10px; }

.panel-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; flex-wrap: wrap; }
.panel-title { font-size: 14px; font-weight: 600; color: var(--text, #2B2B2B); }
.head-actions { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }

.im-select {
  padding: 5px 10px; border: 1px solid var(--border, #d1d5db); border-radius: var(--radius-sm, 8px);
  background: var(--bg-card, #fff); color: var(--text, #2B2B2B); font-size: 13px; min-width: 200px; font-family: inherit;
}

.stat-label { font-size: 12px; text-transform: uppercase; letter-spacing: 0.5px; color: var(--text-muted, #5E5C5F); font-weight: 600; }
.stat-value { font-size: 26px; font-weight: 700; color: var(--text, #2B2B2B); letter-spacing: -0.02em; }

.empty-card { padding: 28px; text-align: center; color: var(--text-muted, #5E5C5F); font-size: 14px; line-height: 1.6; }
.empty-inline { padding-top: 4px; }

.pill { display: inline-block; padding: 2px 10px; border-radius: var(--radius-pill, 20px); font-size: 12px; font-weight: 600; }
.pill-ok { color: #15803d; background: #dcfce7; }
.pill-blue { color: #C7421A; background: #dbeafe; }
.pill-err { color: #b91c1c; background: #fee2e2; }
.pill-cyan { color: #0e7490; background: #cffafe; }
.pill-muted { color: #6b7280; background: #f3f4f6; }
.mt-pill-sim { color: #b45309; background: #ffedd5; }

.prog-wrap { display: flex; align-items: center; gap: 8px; }
.prog-bar { flex: 1; height: 8px; background: var(--border-soft, #EDEDED); border-radius: var(--radius-pill, 20px); overflow: hidden; }
.prog-fill { display: block; height: 100%; background: var(--accent, #E95420); border-radius: var(--radius-pill, 20px); transition: width 0.3s ease; }
.prog-fill.fill-ok { background: #0E8420; }
.prog-text { font-size: 12px; color: var(--text-muted, #5E5C5F); width: 38px; text-align: right; }

.spin { display: inline-block; font-size: 14px; line-height: 1; }
.spin.spinning { animation: spin 0.8s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }
.mono { font-family: var(--mono); }

/* 监控（vLLM /metrics 轻量监控） */
.mt-info-card { padding: 12px 16px; display: flex; align-items: center; gap: 14px; flex-wrap: wrap; }
.mt-info-item { font-size: 14px; font-weight: 600; }
.mt-banner-sim {
  background: #ffedd5; color: #9a3412; border: 1px solid #fdba74;
  padding: 10px 14px; border-radius: var(--radius-sm, 8px); font-size: 13px; font-weight: 600;
}
.mt-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(210px, 1fr)); gap: 14px; }
.im-compact .mt-grid { grid-template-columns: repeat(auto-fill, minmax(160px, 1fr)); gap: 10px; }
.mt-card { padding: 14px 16px; display: flex; flex-direction: column; gap: 8px; }
.im-compact .mt-card { padding: 10px 12px; }
.mt-num { font-size: 24px; }
.im-compact .mt-num { font-size: 20px; }
.mt-unit { font-size: 13px; font-weight: 500; color: var(--text-muted, #5E5C5F); }

/* 网关可路由模型聚合 */
.gw-card { padding: 14px 16px; display: flex; flex-direction: column; gap: 10px; }
.gw-visible-row { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.gw-inst-name { font-size: 14px; font-weight: 600; color: var(--text, #2B2B2B); }
.gw-model-chips { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
.gw-divider { border-top: 1px dashed var(--border-soft, #EDEDED); }
.gw-reason { word-break: break-all; }
</style>
