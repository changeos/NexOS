// =============================================================================
// api.ts —— 流媒体中心应用包的 HTTP 层。
//
// 原 crates/os-api/web/src/api/client.ts 的「流媒体中心段」（endpoints.*）与
// 「直播段」（live 函数 + 类型）原样迁入：端点路径与请求体不变；底层
// fetch/鉴权/超时/错误处理复用宿主 api client（globalThis.__NEXOS_HOST__.api，
// 主前端 appRuntime 注入——应用包不重复实现 HTTP 层）。宿主外直接运行（无桥）
// 时抛出可读错误。
//
// 后端：/api/v1/streaming/*（StreamingRouteHandler）+ /api/v1/live/*
// （LiveRouteHandler，直播 Tab——本地大厅 + 联邦大厅）+ WS /ws/live/:id/:action。
// 注意：live 是直播联邦能力的 UI——引擎端点留在主应用（后端常开、不门控），
// UI 随本应用包走；本包 api 正常调用即可。
// =============================================================================

/** 宿主桥 api 原语（与主前端 client.ts 的 get/post/del/request 同签名）。 */
interface HostApi {
  get<T>(path: string): Promise<T>
  post<T>(path: string, body?: unknown): Promise<T>
  del<T>(path: string): Promise<T>
  request<T>(path: string, opts?: { method?: string; body?: unknown }): Promise<T>
}

/** @nexos/app-sdk 实例形态（类型面；运行时经宿主桥取得，不打包 SDK）。 */
export type AppSdk = import('@nexos/app-sdk').NexosSdk

// 运行时导入（吃狗粮 v0.1.28）：构建期 host-externals 把本导入重写到
// __NEXOS_HOST__.sdk 的工厂面（sdk.createSdk / sdk.SDK_VERSION，零打包）——
// 仅旧宿主（桥上有 api 无 sdk）时才真正调用。
import { createSdk } from '@nexos/app-sdk'

/** hostSdk() 的模块级缓存（旧宿主回退自建实例只建一次，避免多实例缓存漂移）。 */
let cachedSdk: AppSdk | null | undefined

/**
 * 宿主桥上的 @nexos/app-sdk 就绪实例（v0.1.28+ 宿主注入 __NEXOS_HOST__.sdk，
 * 与 ctx.sdk 同一对象）。旧宿主（桥上有 api 无 sdk）用桥上工厂面自建并缓存；
 * 宿主外运行返回 null——调用方自行回退。
 */
export function hostSdk(): AppSdk | null {
  if (cachedSdk !== undefined) return cachedSdk
  const host = (
    globalThis as { __NEXOS_HOST__?: { sdk?: AppSdk; api?: HostApi } }
  ).__NEXOS_HOST__
  if (host?.sdk) {
    cachedSdk = host.sdk
  } else if (host?.api && typeof createSdk === 'function') {
    cachedSdk = createSdk(host.api)
  } else {
    cachedSdk = null
  }
  return cachedSdk
}

/** 取宿主 api（缺失时抛可读错误——本包必须在 NexOS 宿主内运行）。 */
function api(): HostApi {
  const host = (
    globalThis as { __NEXOS_HOST__?: { api?: HostApi } }
  ).__NEXOS_HOST__
  if (!host || !host.api) {
    throw new Error('宿主运行时缺失（__NEXOS_HOST__.api）——请在 NexOS 桌面内运行本应用')
  }
  return host.api
}

// =============================================================================
// 流媒体中心（拉流源 / 多机位 / 转码 / 推流）
// =============================================================================

/** 列全部拉流源（GET /api/v1/streaming/sources）。 */
export function streamingSources(): Promise<unknown> {
  return api().get('/api/v1/streaming/sources')
}

/** 添加拉流源（POST /api/v1/streaming/sources，需 admin）。 */
export function addStreamingSource(body: unknown): Promise<unknown> {
  return api().post('/api/v1/streaming/sources', body)
}

/** 删除拉流源（DELETE /api/v1/streaming/sources/:id，需 admin）。 */
export function deleteStreamingSource(id: string): Promise<unknown> {
  return api().del(`/api/v1/streaming/sources/${encodeURIComponent(id)}`)
}

/** 开始录制（POST /api/v1/streaming/sources/:id/record/start，需 admin）。 */
export function startRecording(id: string): Promise<unknown> {
  return api().post(`/api/v1/streaming/sources/${encodeURIComponent(id)}/record/start`, {})
}

/** 停止录制（POST /api/v1/streaming/sources/:id/record/stop，需 admin）。 */
export function stopRecording(id: string): Promise<unknown> {
  return api().post(`/api/v1/streaming/sources/${encodeURIComponent(id)}/record/stop`, {})
}

/** 节目输出主源（GET /api/v1/streaming/program）。 */
export function streamingProgram(): Promise<unknown> {
  return api().get('/api/v1/streaming/program')
}

/** 切换节目主输出源（POST /api/v1/streaming/program/switch，需 admin）。 */
export function switchProgram(sourceId: string): Promise<unknown> {
  return api().post('/api/v1/streaming/program/switch', { source_id: sourceId })
}

/** 列全部转码任务（GET /api/v1/streaming/transcode）。 */
export function streamingTranscodes(): Promise<unknown> {
  return api().get('/api/v1/streaming/transcode')
}

/** 可用本地视频文件（GET /api/v1/streaming/transcode/sources）—— 转码输入源选择。 */
export function streamingTranscodeSources(): Promise<unknown> {
  return api().get('/api/v1/streaming/transcode/sources')
}

/** 创建转码任务（POST /api/v1/streaming/transcode，需 admin）。 */
export function createTranscode(body: unknown): Promise<unknown> {
  return api().post('/api/v1/streaming/transcode', body)
}

/** 删除转码任务（DELETE /api/v1/streaming/transcode/:id，需 admin）。 */
export function deleteTranscode(id: string): Promise<unknown> {
  return api().del(`/api/v1/streaming/transcode/${encodeURIComponent(id)}`)
}

/** 列全部推流目标（GET /api/v1/streaming/outputs）。 */
export function streamingOutputs(): Promise<unknown> {
  return api().get('/api/v1/streaming/outputs')
}

/** 添加推流目标（POST /api/v1/streaming/outputs，需 admin）。 */
export function addStreamingOutput(body: unknown): Promise<unknown> {
  return api().post('/api/v1/streaming/outputs', body)
}

/** 删除推流目标（DELETE /api/v1/streaming/outputs/:id，需 admin）。 */
export function deleteStreamingOutput(id: string): Promise<unknown> {
  return api().del(`/api/v1/streaming/outputs/${encodeURIComponent(id)}`)
}

/** 启动推流（POST /api/v1/streaming/outputs/:id/start，需 admin）。 */
export function startStreamingOutput(id: string): Promise<unknown> {
  return api().post(`/api/v1/streaming/outputs/${encodeURIComponent(id)}/start`, {})
}

/** 停止推流（POST /api/v1/streaming/outputs/:id/stop，需 admin）。 */
export function stopStreamingOutput(id: string): Promise<unknown> {
  return api().post(`/api/v1/streaming/outputs/${encodeURIComponent(id)}/stop`, {})
}

/** 流媒体统计（GET /api/v1/streaming/stats）。 */
export function streamingStats(): Promise<unknown> {
  return api().get('/api/v1/streaming/stats')
}

/** 兼容主前端消费形态（StreamingCenter.vue 原样迁入：`endpoints.xxx(...)` 零改动）。 */
export const endpoints = {
  streamingSources,
  addStreamingSource,
  deleteStreamingSource,
  startRecording,
  stopRecording,
  streamingProgram,
  switchProgram,
  streamingTranscodes,
  streamingTranscodeSources,
  createTranscode,
  deleteTranscode,
  streamingOutputs,
  addStreamingOutput,
  deleteStreamingOutput,
  startStreamingOutput,
  stopStreamingOutput,
  streamingStats,
}

// =============================================================================
// 直播（live：本地大厅 + 联邦大厅，REST 3 条 + WS 2 条，handlers/live.rs）
//
// 浏览器采集（getUserMedia/getDisplayMedia + MediaRecorder webm chunk）→
// WS 上行 → 服务端内存扇出 → 观众 WS 下行 → MSE 播放。
// 房间状态纯内存（服务重启即清空，无演示房间）；viewer_count/bytes_* 全为
// 服务端真实计数。联邦：房间经 overlay 宣告（live_lobby）合并进联邦大厅，
// 观看远端房间经中继（live_relay_*）注入本节点影子房间——WS view 端点
// 对本地/联邦形态房间 id 无感知差异。契约见 docs/LIVE_STREAMING.md。
// =============================================================================

/** 直播房间（GET /api/v1/live/rooms 响应 local 数组元素；全字段真实值）。 */
export interface LiveRoom {
  id: string
  title: string
  /** `"screen"` | `"camera"` */
  source_kind: string
  created_at: string
  /** 创建者身份（admin Principal 用户名）。 */
  publisher_identity: string
  /** 真实观众连接数。 */
  viewer_count: number
  /** `"live"` | `"ended"` */
  status: string
  /** 主播上行累计字节。 */
  bytes_in: number
  /** 观众下行累计字节（只计成功投递）。 */
  bytes_out: number
  /** 慢消费者丢帧数。 */
  dropped_frames: number
  /** 上行超限拒收帧数（> 2 MiB/帧默认上限）。 */
  rejected_frames: number
  /** 主播 WS 是否在线。 */
  publisher_online: boolean
  /** 是否已缓存 init segment（观众中途加入可重放）。 */
  header_cached: boolean
}

/**
 * 联邦房间（GET /api/v1/live/rooms 响应 federated 数组元素）：远端节点
 * live_lobby 宣告按 id（`<节点短前缀>:<room_id>`）幂等合并的条目（TTL 90s）。
 */
export interface FederatedLiveRoom {
  /** 联邦形态房间 id（观看走同一 WS view 端点——本节点中继注入影子房间）。 */
  id: string
  title: string
  /** `"screen"` | `"camera"` */
  source_kind: string
  /** `"live"`（ended 即出表）。 */
  status: string
  /** 源节点 NodeID（`0x` + 66 hex）。 */
  node_id: string
  /** 源节点名（展示）。 */
  node_name: string
  /** 源节点本地观众数（宣告快照）。 */
  viewer_count: number
  /** 源节点主播是否在线（宣告快照）。 */
  publisher_online: boolean
  /** 宣告时间（ISO 串）。 */
  updated_at: string
}

/**
 * 两段式直播大厅（GET /api/v1/live/rooms，公开读）：
 * local = 本节点房间（可开播）；federated = 联邦宣告合并的远端房间。
 */
export interface LiveRoomsLobby {
  local: LiveRoom[]
  federated: FederatedLiveRoom[]
}

/** 创建房间请求体（POST /api/v1/live/rooms，需 admin）。 */
export interface LiveCreateRoomBody {
  title: string
  source_kind: 'screen' | 'camera'
}

/** 创建房间响应（201；房间视图 + publish token——仅此一次下发，列表不回）。 */
export interface LiveRoomCreated extends LiveRoom {
  publish_token: string
}

/** 创建直播房间（POST /api/v1/live/rooms，admin）→ 房间 + publish token。 */
export function liveCreateRoom(body: LiveCreateRoomBody): Promise<LiveRoomCreated> {
  return api().post<LiveRoomCreated>('/api/v1/live/rooms', body)
}

/** 直播两段式大厅（GET /api/v1/live/rooms，公开读）：{local, federated}。 */
export function liveListRooms(): Promise<LiveRoomsLobby> {
  return api().get<LiveRoomsLobby>('/api/v1/live/rooms')
}

/** 结束直播（DELETE /api/v1/live/rooms/:id，admin；踢断主播与全部观众，房间出表）。 */
export function liveEndRoom(id: string): Promise<LiveRoom> {
  return api().del<LiveRoom>(`/api/v1/live/rooms/${encodeURIComponent(id)}`)
}

/**
 * 直播 WS URL 构造（同源 ws/wss 自适应，同主前端 AdminConsole 终端 WS 模式）：
 * - publish：`?token=` 必填（创建房间返回的 publish token，服务端精确匹配 401）
 * - view：公开（不拼 token）；本地与联邦形态房间 id 同一端点（联邦房间由
 *   本节点中继注入影子房间，前端无感知差异）
 */
export function liveWsUrl(roomId: string, action: 'publish' | 'view', token?: string): string {
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  const qs = action === 'publish' && token ? `?token=${encodeURIComponent(token)}` : '';
  return `${proto}://${location.host}/ws/live/${encodeURIComponent(roomId)}/${action}${qs}`;
}
