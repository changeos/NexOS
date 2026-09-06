// =============================================================================
// api.ts —— 二维码传输应用包的 HTTP 层。
//
// 原 crates/os-api/web/src/api/client.ts 的二维码段（endpoints 函数）原样迁入：
// 端点路径与请求体不变；底层 fetch/鉴权/超时/错误处理复用宿主 api client
// （globalThis.__NEXOS_HOST__.api，主前端 appRuntime 注入——应用包不重复实现
// HTTP 层）。宿主外直接运行（无桥）时抛出可读错误。
//
// 后端：/api/v1/qr/*（QrTransferRouteHandler）。主前端 client.ts 的对应段已随
// 本应用剥离移除；主前端内唯一的跨应用消费方 BleHub（mesh 连接 QR）改为直接
// 调用同名端点，/api/v1/qr/* 后端常开。
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

/** API 错误（与主前端 client.ts ApiError 同形态：status + path，QrTransfer.vue
 *  的 friendlyError 做 instanceof 分支展示）。 */
export class ApiError extends Error {
  status?: number;
  path?: string;

  constructor(message: string, init?: { status?: number; path?: string }) {
    super(message);
    this.name = 'ApiError';
    if (init) {
      this.status = init.status;
      this.path = init.path;
    }
  }
}

// =============================================================================
// 二维码文件传输（文件 → 跳动 QR 视频 + 解码回文件）
// =============================================================================

/** 编码文件为 QR 视频（POST /api/v1/qr/encode，需 admin）。 */
export function qrEncode(body: unknown): Promise<unknown> {
  return api().post('/api/v1/qr/encode', body)
}

/** 编码任务状态 + 视频 URL（GET /api/v1/qr/encode/:id）。 */
export function qrEncodeStatus(id: string): Promise<unknown> {
  return api().get(`/api/v1/qr/encode/${encodeURIComponent(id)}`)
}

/** 下载/流式播放 QR 视频（GET /api/v1/qr/encode/:id/video，返回 video_url 信封）。 */
export function qrEncodeVideo(id: string): Promise<unknown> {
  return api().get(`/api/v1/qr/encode/${encodeURIComponent(id)}/video`)
}

/** 解码（POST /api/v1/qr/decode，需 admin，上传视频/图片 → 文件）。 */
export function qrDecode(body: unknown): Promise<unknown> {
  return api().post('/api/v1/qr/decode', body)
}

/** 解码任务状态 + 输出文件路径（GET /api/v1/qr/decode/:id）。 */
export function qrDecodeStatus(id: string): Promise<unknown> {
  return api().get(`/api/v1/qr/decode/${encodeURIComponent(id)}`)
}

/** 下载解码后的文件（GET /api/v1/qr/decode/:id/file，返回 output_url 信封）。 */
export function qrDecodeFile(id: string): Promise<unknown> {
  return api().get(`/api/v1/qr/decode/${encodeURIComponent(id)}/file`)
}

/** 二维码文件传输统计（GET /api/v1/qr/stats）。 */
export function qrStats(): Promise<unknown> {
  return api().get('/api/v1/qr/stats')
}

// =============================================================================
// 二维码文本传输（文本 ⇄ QR 图片，即时，无视频）
// =============================================================================

/** 文本 → QR 图片（POST /api/v1/qr/encode-text，需 admin）。
 *  body: { text, error_level? } → { qr_count, qr_images: [base64...], original_size, compressed_size }。 */
export function qrEncodeText(text: string, errorLevel = 'L'): Promise<unknown> {
  return api().post('/api/v1/qr/encode-text', { text, error_level: errorLevel })
}

/** QR 图片 → 文本（POST /api/v1/qr/decode-text，需 admin）。 */
export function qrDecodeText(imageBase64: string): Promise<unknown> {
  return api().post('/api/v1/qr/decode-text', { image_base64: imageBase64 })
}

// =============================================================================
// 宿主通用端点（源文件路径选择器用；与 film 包附带 llm/instances 同理）
// =============================================================================

/** 列目录（GET /api/v1/files/list[?path=]。path 为空映射到根（/tank））——
 *  编码 Tab「从文件管理器选…」的路径选择器数据源。 */
export function filesList(path?: string): Promise<unknown> {
  return api().get(`/api/v1/files/list${path ? '?path=' + encodeURIComponent(path) : ''}`)
}

/** 兼容主前端消费形态（views 原样迁入：`endpoints.qrXxx(...)` 调用零改动）。 */
export const endpoints = {
  qrEncode,
  qrEncodeStatus,
  qrEncodeVideo,
  qrDecode,
  qrDecodeStatus,
  qrDecodeFile,
  qrStats,
  qrEncodeText,
  qrDecodeText,
  filesList,
}
