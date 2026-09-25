// =============================================================================
// api.ts —— 影片制作应用包的 HTTP 层。
//
// 原 crates/os-api/web/src/api/client.ts 的 film 段（类型 + 端点函数）原样迁入：
// 端点路径与请求体不变；底层 fetch/鉴权/超时/错误处理复用宿主 api client
// （globalThis.__NEXOS_HOST__.api，主前端 appRuntime 注入——应用包不重复实现
// HTTP 层）。宿主外直接运行（无桥）时抛出可读错误。
//
// 后端：/api/v1/film/*（crates/os-api/src/handlers/film.rs）。
// 模型源下拉还需要两个宿主端点（llm/instances、gateway/channels），一并封装。
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

/** 模型引用：本地能力（chat=运行中 LLM 实例 / image=本地 sd-turbo）或网关渠道。 */
export interface FilmModelRef {
  /** local=本地能力；channel=网关渠道（channel_id 必带）。 */
  source: 'local' | 'channel'
  /** source=channel 时的网关渠道 id。 */
  channel_id?: string
  /** 能力面：chat 剧本 / image 分镜图 / video 图生视频 / tts 配音 / music 音乐。 */
  capability: 'chat' | 'image' | 'video' | 'tts' | 'music'
  /** 上游模型名（v0.1.38 项目默认配置/临时覆盖均可带；local.chat 忽略）。 */
  model?: string
}

// —— 项目级模型设置（v0.1.38：八能力位默认模型源，models.json 文件即真值） ——

/** 八能力位（冻结枚举；asr/emb/vl 为预留槽位）。 */
export const FILM_MODEL_CAPS = [
  'chat',
  'image',
  'video',
  'tts',
  'music',
  'asr',
  'emb',
  'vl',
] as const
export type FilmModelCap = (typeof FILM_MODEL_CAPS)[number]

/** 预留槽位（配置就绪、待消费场景——前端灰提示；v0.1.39.1 emb 入消费位）。 */
export const FILM_MODEL_CAPS_RESERVED: readonly FilmModelCap[] = ['asr', 'vl']

/** models.json 单能力位路由规则（v0.1.44：task 缺省=兜底路由；列表顺序=
 * fallback 顺序，全失败后回落主默认）。 */
export interface FilmModelRoute {
  /** 任务名（如 story.clean / story 前缀；缺省=兜底路由）。 */
  task?: string | null
  source?: 'local' | 'channel' | null
  channel_id?: string | null
  model?: string | null
  note?: string | null
  /** GET 快照附带的逐条可用性探测（不发网络请求）。 */
  available?: { ok?: boolean; detail?: string } | null
}

/** models.json 单能力位配置（source=null 即未设置）。 */
export interface FilmModelSlot {
  source?: 'local' | 'channel' | null
  channel_id?: string | null
  model?: string | null
  note?: string | null
  /** 高级路由规则（v0.1.44；空/缺省=无路由，走主默认）。 */
  routes?: FilmModelRoute[]
}

/** GET :id/models 单能力位快照（槽位平铺 + 探测 + 预留标记；宽松字段）。 */
export interface FilmModelCapView extends FilmModelSlot {
  capability: FilmModelCap
  /** true=预留槽位（待消费场景）。 */
  reserved?: boolean
  /** true=已消费（生成动作缺省链接入）。 */
  consumed?: boolean
  /** 可用性探测快照（不发网络请求）。 */
  available?: { ok?: boolean; detail?: string } | null
}

/** GET :id/models 响应。 */
export interface FilmProjectModels {
  path?: string
  capabilities?: FilmModelCapView[]
  [k: string]: unknown
}

/** 项目模型配置快照（GET /api/v1/film/projects/:id/models）。 */
export function filmGetProjectModels(projectId: string): Promise<FilmProjectModels> {
  return api().get<FilmProjectModels>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/models`,
  )
}

/** 单能力位更新（PUT …/models {capability, source:local|channel|null(清除),
 *  channel_id?, model?, note?, routes?, author?} → 写 models.json + activity。
 *  v0.1.44：routes 传数组（含 []）=整组替换，缺省=保留现有路由）。 */
export function filmSetProjectModel(
  projectId: string,
  body: {
    capability: FilmModelCap
    source?: 'local' | 'channel' | null
    channel_id?: string
    model?: string
    note?: string
    routes?: FilmModelRoute[]
    author?: string
  },
): Promise<{ capability?: string; slot?: FilmModelSlot; [k: string]: unknown }> {
  return api().request(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/models`,
    { method: 'PUT', body },
  )
}

/** 单个镜头（分镜）——后端 script 数组元素（字段名与 film.rs ScriptShot 对齐）。 */
export interface FilmShot {
  /** 镜头序号（1 起，URL 路径参数 :n 即此值）。 */
  shot: number
  /** 镜头描述。 */
  desc?: string | null
  /** 图像生成提示词。 */
  image_prompt?: string | null
  /** 视频生成提示词。 */
  video_prompt?: string | null
  /** 台词（配音文本）。 */
  line?: string | null
  /** 时长（秒）。 */
  duration_secs?: number | null
  /** 出场角色名数组（角色库绑定；0.1.29+ 后端返回，旧后端缺省 undefined）。 */
  characters?: string[] | null
  /** 绑定动作对象名数组（0.1.41 P0：分镜 actions 引用 → 生成链注入 motion/camera）。 */
  actions?: string[] | null
}

/** 产物/参考图清单元素（GET 项目详情 artifacts/refs）。 */
export interface FilmArtifact {
  name: string
  bytes: number
}

/** 影片项目（GET/POST /api/v1/film/projects 系列）。 */
export interface FilmProject {
  id: string
  title: string
  /** 创意描述（剧本生成的输入）。 */
  idea: string
  /** 画面比例：16:9 / 9:16 / 1:1 / 2.39:1 / 1.85:1 / 4:3（六档预设，v0.1.37；
   * 前端预设表与合成分辨率映射见 flow/flowTypes.ts RATIO_PRESETS）。 */
  ratio: string
  /** 风格提示（可选）。 */
  style_hint?: string | null
  /** 产物目录绝对路径（files/download 读取路径拼接用）。 */
  dir: string
  /** 导出目录（v0.1.34 契约：PUT export_dir 设置；空串=重置为项目目录）。 */
  export_dir?: string | null
  /** 成片落盘绝对路径（缺省 <项目目录>/final.mp4；详情响应回显）。 */
  final_path?: string | null
  /** `draft / scripted / producing / done`。 */
  status?: string
  /** 分镜列表（生成剧本后非空；详情/PUT 响应回显）。 */
  script?: FilmShot[] | null
  /** 产物文件清单（详情响应；shot-N.png 等文件名）。 */
  artifacts?: FilmArtifact[] | null
  /** 项目参考图清单（refs/ 目录；0.1.29+）。 */
  refs?: FilmArtifact[] | null
  created_at: string
  updated_at: string
}

/** GET /api/v1/film/projects/:id 响应（项目行 + 分镜 + 产物清单 + refs）。 */
export interface FilmProjectDetail {
  project: FilmProject
  script?: FilmShot[] | null
  artifacts?: FilmArtifact[] | null
  refs?: FilmArtifact[] | null
}

/** POST /api/v1/film/projects body。 */
export interface FilmProjectCreateBody {
  title: string
  idea: string
  ratio: string
  style_hint?: string
}

/** PUT /api/v1/film/projects/:id body（项目字段可选；script 支持局部镜头保存——
 *  数组元素只须带 shot（或 index）+ 变更字段，后端按镜头号合并进既有分镜）。 */
export interface FilmProjectUpdateBody {
  title?: string
  idea?: string
  ratio?: string
  style_hint?: string | null
  /** 导出目录（空串=重置为项目目录；须为已存在的绝对路径，400 校验在后端）。 */
  export_dir?: string
  /** 局部分镜保存（镜头编辑面板「保存」按钮 / 角色绑定编辑）。 */
  script?: Partial<FilmShot & { index?: number }>[]
}

/** 影片角色（GET/POST /api/v1/film/projects/:id/characters 等）。 */
export interface FilmCharacter {
  /** `char-<n>`（项目内唯一）。 */
  id: string
  name: string
  /** 外观/设定描述（image prompt 注入与定妆图生成共用）。 */
  description: string
  /** TTS 音色（OpenAI voice 枚举或渠道 voice_id；空=落全局缺省）。 */
  voice?: string | null
  /** 定妆图产物相对路径（相对项目 dir）。 */
  portrait_ref?: string | null
  /** 定妆图读取 URL（files/download b64 信封；取 content_base64 转 data URL）。 */
  portrait_url?: string | null
  /** 绑定镜头号清单（1 起）。 */
  bound_shots?: number[]
  created_at: string
  updated_at: string
}

/** 生成类请求 body 公共段（各生成端点共用；model_ref v0.1.38 起可省——
 *  缺省读项目 models.json 能力位默认源，「项目默认」下拉项即不传本字段）。 */
export interface FilmGenBody {
  model_ref?: FilmModelRef
  /** 可选文本覆盖（image/video 覆盖对应 prompt；tts 覆盖台词）。 */
  text?: string
}

/**
 * 生成分镜可选配置（v0.1.39 生成分镜配置面板）：出场人物约束 / 声线 /
 * 镜头数（5-12，缺省自动）/ 总时长提示（自由文本，如「约 60 秒」）。
 *
 * v0.1.44 按章分镜：chapter_range（"3" 单章 / "3-5" 范围 / "3,7,9" 枚举——
 * 后端宽松解析分隔符 ,-~）给定 → 语料读 story/chapters/ch-<NN>.md 不再喂
 * 全文；未给 = story.md 全文（现行为）。章节卡「从此章生成分镜」经
 * pendingStoryboardChapter 预填面板下拉后随本字段发出。
 */
export interface FilmStoryboardGenOptions {
  /** 出场人物约束（casting/characters 名单；注入【出场角色】语义）。 */
  characters?: string[]
  /** 人物声线（name → voice；面板已在勾选时落 card，此字段供后端提示词用）。 */
  voices?: Record<string, string>
  /** 镜头数（5-12；缺省=自动 5-12）。 */
  shot_count?: number
  /** 总时长提示（自由文本，如「约 60 秒」）。 */
  duration_hint?: string
  /** 章节范围（v0.1.44 按章分镜；空=全文语料）。 */
  chapter_range?: string
}

/** 生成配置字段 → body 展开（空段不带字段——未配置时请求与旧契约一致）。 */
function storyboardGenBodyFields(opts?: FilmStoryboardGenOptions): Record<string, unknown> {
  if (!opts) return {}
  const out: Record<string, unknown> = {}
  if (opts.characters?.length) out.characters = [...opts.characters]
  if (opts.voices && Object.keys(opts.voices).length) out.voices = { ...opts.voices }
  if (typeof opts.shot_count === 'number') out.shot_count = opts.shot_count
  if (opts.duration_hint && opts.duration_hint.trim()) {
    out.duration_hint = opts.duration_hint.trim()
  }
  if (opts.chapter_range && opts.chapter_range.trim()) {
    out.chapter_range = opts.chapter_range.trim()
  }
  return out
}

/** 影片任务（生命周期 queued→running→done|error；log 为环形日志尾）。
 *  前端统一口径：status 映射 completed/failed（后端 done/error）。 */
export interface FilmTask {
  id: string
  /** script / image / video / tts / music / compose / portrait。 */
  kind: 'script' | 'image' | 'video' | 'tts' | 'music' | 'compose' | 'portrait' | string
  /** queued / running / completed / failed（后端 done/error 映射）。 */
  status: 'queued' | 'running' | 'completed' | 'failed' | string
  /** 后端原始状态（done/error），诊断用。 */
  raw_status?: string
  project_id?: string | null
  /** 环形日志尾（按序）。 */
  log?: string[]
  /** 产物路径（done 时）。 */
  output?: string | null
  /** 失败原因（status=failed 时）。 */
  error?: string | null
  created_at: number | string
  finished_at?: number | null
}

/** 创建影片项目（POST /api/v1/film/projects）→ 项目。 */
export function filmCreateProject(body: FilmProjectCreateBody): Promise<FilmProject> {
  return api().post<FilmProject>('/api/v1/film/projects', body)
}

/** 项目列表（GET /api/v1/film/projects）。 */
export function filmListProjects(): Promise<FilmProject[]> {
  return api().get<FilmProject[]>('/api/v1/film/projects')
}

/** 项目详情（GET /api/v1/film/projects/:id）→ {project, script, artifacts, refs}。 */
export function filmGetProject(id: string): Promise<FilmProjectDetail> {
  return api().get<FilmProjectDetail>(`/api/v1/film/projects/${encodeURIComponent(id)}`)
}

/** 更新项目（PUT /api/v1/film/projects/:id；script 支持局部保存——后端按
 *  镜头号合并，数组元素只须带 shot/index + 变更字段）。 */
export function filmUpdateProject(
  id: string,
  body: FilmProjectUpdateBody,
): Promise<FilmProject> {
  return api().request<FilmProject>(`/api/v1/film/projects/${encodeURIComponent(id)}`, {
    method: 'PUT',
    body,
  })
}

/** 删除项目（DELETE /api/v1/film/projects/:id，含全部产物）。 */
export function filmDeleteProject(id: string): Promise<null> {
  return api().del<null>(`/api/v1/film/projects/${encodeURIComponent(id)}`)
}

/** 生成分镜（POST /api/v1/film/projects/:id/script {model_ref?, …配置?}）→ 任务。
 *  v0.1.39 语义更名：产出即分镜脚本（分镜=storyboard）；工作台「生成分镜」
 *  按钮走本端点（与 storyboard/generate 同一执行体的兼容别名）。opts 为
 *  生成分镜配置面板拼装的可选字段（后端 v1 未消费——见 FilmStoryboardGenOptions）。 */
export function filmGenScript(
  id: string,
  modelRef?: FilmModelRef,
  opts?: FilmStoryboardGenOptions,
): Promise<FilmTask> {
  return api().post<FilmTask>(`/api/v1/film/projects/${encodeURIComponent(id)}/script`, {
    ...(modelRef ? { model_ref: modelRef } : {}),
    ...storyboardGenBodyFields(opts),
  } satisfies FilmGenBody & Record<string, unknown>)
}

/** 生成镜头分镜图（POST …/shots/:n/image {model_ref?, text?}）→ 任务。 */
export function filmGenShotImage(
  id: string,
  n: number,
  modelRef?: FilmModelRef,
  text?: string,
): Promise<FilmTask> {
  return api().post<FilmTask>(
    `/api/v1/film/projects/${encodeURIComponent(id)}/shots/${n}/image`,
    {
      ...(modelRef ? { model_ref: modelRef } : {}),
      ...(text ? { text } : {}),
    },
  )
}

/** 图生视频（POST …/shots/:n/video {model_ref?, text?}）→ 任务。 */
export function filmGenShotVideo(
  id: string,
  n: number,
  modelRef?: FilmModelRef,
  text?: string,
): Promise<FilmTask> {
  return api().post<FilmTask>(
    `/api/v1/film/projects/${encodeURIComponent(id)}/shots/${n}/video`,
    {
      ...(modelRef ? { model_ref: modelRef } : {}),
      ...(text ? { text } : {}),
    },
  )
}

/** 生成镜头配音（POST …/shots/:n/tts {model_ref?, text?}；text 缺省用台词）→ 任务。 */
export function filmGenShotTts(
  id: string,
  n: number,
  modelRef?: FilmModelRef,
  text?: string,
): Promise<FilmTask> {
  return api().post<FilmTask>(
    `/api/v1/film/projects/${encodeURIComponent(id)}/shots/${n}/tts`,
    {
      ...(modelRef ? { model_ref: modelRef } : {}),
      ...(text ? { text } : {}),
    },
  )
}

/** 生成 BGM（POST /api/v1/film/projects/:id/music {model_ref?, prompt?}）→ 任务。 */
export function filmGenMusic(
  id: string,
  modelRef?: FilmModelRef,
  prompt?: string,
): Promise<FilmTask> {
  return api().post<FilmTask>(`/api/v1/film/projects/${encodeURIComponent(id)}/music`, {
    ...(modelRef ? { model_ref: modelRef } : {}),
    ...(prompt ? { prompt } : {}),
  })
}

/**
 * 合成成片（POST /api/v1/film/projects/:id/compose；v0.1.35 契约 body 可含
 * {bgm_track}——音频页 BGM 库选择联动；产物 = dist/final-v*.mp4 版本列表）→ 任务。
 */
export function filmCompose(id: string, bgmTrack?: string, author?: string): Promise<FilmTask> {
  return api().post<FilmTask>(`/api/v1/film/projects/${encodeURIComponent(id)}/compose`, {
    ...(bgmTrack ? { bgm_track: bgmTrack } : {}),
    ...(author ? { author } : {}),
  })
}

/** 任务轮询（GET /api/v1/film/tasks/:id；log 环形日志尾，未知 id 抛 404）。 */
export function filmGetTask(taskId: string): Promise<FilmTask> {
  return api().get<FilmTask>(`/api/v1/film/tasks/${encodeURIComponent(taskId)}`)
}

// —— 短片生成 ShotGen（v0.1.45：直连 video 渠道的单段生成器；不建项目不走
//    分镜/生图/合成管线——对标海螺 H3 创作页形态。后端契约并行开发中：
//    接口失败如实展示（列表空态/提交红条），不崩）——

/** 短片生成作品（GET /film/shotgen 列表与 :id 详情元素；宽松字段——
 *  详情为任务态 + 作品字段合并形态）。 */
export interface FilmShotgenWork {
  id: string
  prompt?: string | null
  ratio?: string | null
  duration_secs?: number | null
  seed?: number | null
  /** queued / running / done | error（后端原始态；前端归一 completed/failed）。 */
  status?: string
  /** 所用模型/渠道名（回显；缺省 ''）。 */
  model?: string | null
  /** 产物视频（http(s) URL 或产物路径——路径经 files/download 信封转 data URL）。 */
  video_url?: string | null
  video_path?: string | null
  /** 参考图回显（图生视频档；可缺省）。 */
  image_url?: string | null
  created_at?: number | string | null
  finished_at?: number | string | null
  error?: string | null
  log?: string[]
  [k: string]: unknown
}

/** POST /api/v1/film/shotgen body（model_ref 缺省走 video 能力位默认源）。 */
export interface FilmShotgenCreateBody {
  prompt: string
  ratio: string
  duration_secs: number
  /** 随机种子（0 / 缺省 = 随机）。 */
  seed?: number
  /** 图生视频参考图（标准 b64，无 data: 前缀）。 */
  image_b64?: string
  model_ref?: FilmModelRef
}

/** 提交短片生成（POST /api/v1/film/shotgen → 202 任务，stage=shotgen）。 */
export function filmShotgenCreate(body: FilmShotgenCreateBody): Promise<FilmTask> {
  return api().post<FilmTask>('/api/v1/film/shotgen', body)
}

/** 作品列表（GET /api/v1/film/shotgen——历史持久化服务端，新→旧）。 */
export function filmShotgenList(): Promise<FilmShotgenWork[]> {
  return api().get<FilmShotgenWork[]>('/api/v1/film/shotgen')
}

/** 作品详情（GET /api/v1/film/shotgen/:id → 任务详情 + 产物 url；轮询用）。 */
export function filmShotgenGet(id: string): Promise<FilmShotgenWork> {
  return api().get<FilmShotgenWork>(`/api/v1/film/shotgen/${encodeURIComponent(id)}`)
}

// =============================================================================
// 模型源下拉数据（宿主通用端点，宽松字段）
//
// v0.1.28 起优先走 @nexos/app-sdk（hostSdk()——sdk.llm.instances() /
// sdk.gateway.channels()）；旧宿主（无桥 sdk）回退本文件的手拼端点。
// =============================================================================

/** LLM 实例列表（GET /api/v1/llm/instances；chat 源「本地」组）。 */
export function fetchLlmInstances(): Promise<unknown> {
  return api().get('/api/v1/llm/instances')
}

/** 网关渠道列表（GET /api/v1/gateway/channels；各能力面「渠道」组）。 */
export function fetchGatewayChannels(): Promise<unknown> {
  return api().get('/api/v1/gateway/channels')
}

/** 经 SDK 取本地 LLM 实例列表（无 SDK 返回 null——调用方回退 fetch 版）。 */
export async function sdkLlmInstances(): Promise<unknown[] | null> {
  const sdk = hostSdk()
  if (!sdk) return null
  return (await sdk.llm.instances()) as unknown[]
}

/** 经 SDK 取网关渠道列表（无 SDK 返回 null）。 */
export async function sdkGatewayChannels(): Promise<unknown[] | null> {
  const sdk = hostSdk()
  if (!sdk) return null
  return (await sdk.gateway.channels()) as unknown[]
}

// —— 渠道直填创建（v0.1.39 模型设置页「+ 添加 API」） ——

/** 渠道创建响应（POST /api/v1/gateway/channels → 201 Channel 子集；宽松字段，
 *  api_key 后端脱敏不回显）。 */
export interface GatewayChannelCreated {
  id?: string
  name?: string
  provider?: string
  base_url?: string
  models?: string[]
  enabled?: boolean
  status?: string
  [k: string]: unknown
}

/**
 * 直填创建网关渠道（POST /api/v1/gateway/channels，admin；SDK 网关面只有
 * channels()/chat 无创建——走宿主 api 原语）。
 *
 * 后端契约（crates/os-api/src/handlers/api_gateway.rs CreateChannelBody）：
 * `{name, provider, base_url, api_key?, models?, priority?, weight?}` → 201
 * Channel。校验：name/base_url 非空、provider 白名单
 * openai/deepseek/anthropic/local-vllm/azure/ollama。渠道**无 capability/
 * 标签字段**——能力位归属由项目 models.json slot（channel_id）承载，故本
 * 封装不带能力参数；直填口径固定 provider='openai'（OpenAI 兼容协议缺省，
 * 网关转发后缀 /chat/completions 等按 OpenAI 形态直拼）。
 */
export function createGatewayChannel(body: {
  name: string
  provider: string
  base_url: string
  api_key?: string
  models?: string[]
}): Promise<GatewayChannelCreated> {
  return api().post<GatewayChannelCreated>('/api/v1/gateway/channels', body)
}

/**
 * 直填 API 地址规整（前端代行——后端 base_url 原样存储直拼后缀，不补 /v1）：
 * 去首尾空白与尾部 `/`；裸 scheme://host[:port]（无路径）补 `/v1`（OpenAI
 * 兼容端点路径约定）；用户已带路径（`…/v1`、`…/openai/v1` 等）原样保留。
 */
export function normalizeApiBaseUrl(raw: string): string {
  const url = raw.trim().replace(/\/+$/, '')
  if (!url) return ''
  try {
    const u = new URL(url)
    if (!u.pathname || u.pathname === '/') return `${u.origin}/v1`
  } catch {
    // 非 URL 形态（后端会给 400 校验）——原样透传
  }
  return url
}

// =============================================================================
// 角色库与参考导入（2026-09-04 P0 一致性；os-api 0.1.29+ 端点——旧后端 404
// 如实展示，角色区置灰不崩）
// =============================================================================

/** OpenAI 标准 voice 枚举（11 个；角色卡下拉预设，另支持自定义输入）。 */
export const OPENAI_VOICES = [
  'alloy',
  'ash',
  'ballad',
  'coral',
  'echo',
  'fable',
  'onyx',
  'nova',
  'sage',
  'shimmer',
  'verse',
] as const

/** 角色列表（GET /api/v1/film/projects/:id/characters；含 portrait_url/绑定镜头）。 */
export function filmListCharacters(projectId: string): Promise<FilmCharacter[]> {
  return api().get<FilmCharacter[]>(`/api/v1/film/projects/${encodeURIComponent(projectId)}/characters`)
}

/** 建角色（POST …/characters；name+description 必填，voice 可选）。 */
export function filmCreateCharacter(
  projectId: string,
  body: { name: string; description: string; voice?: string },
): Promise<FilmCharacter> {
  return api().post<FilmCharacter>(`/api/v1/film/projects/${encodeURIComponent(projectId)}/characters`, body)
}

/** 改角色（PUT /api/v1/film/characters/:cid；部分更新，voice 传空串=清空）。 */
export function filmUpdateCharacter(
  characterId: string,
  body: { name?: string; description?: string; voice?: string },
): Promise<FilmCharacter> {
  return api().request<FilmCharacter>(`/api/v1/film/characters/${encodeURIComponent(characterId)}`, {
    method: 'PUT',
    body,
  })
}

/** 删角色（DELETE /api/v1/film/characters/:cid，连定妆图目录）。 */
export function filmDeleteCharacter(characterId: string): Promise<{ deleted: string }> {
  return api().del(`/api/v1/film/characters/${encodeURIComponent(characterId)}`)
}

/** 上传定妆图（POST …/characters/:cid/portrait；b64 ≤10MB，png/jpeg/webp）。 */
export function filmUploadPortrait(
  projectId: string,
  characterId: string,
  imageB64: string,
  mime?: string,
): Promise<FilmCharacter> {
  return api().post<FilmCharacter>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/characters/${encodeURIComponent(characterId)}/portrait`,
    mime ? { image_b64: imageB64, mime } : { image_b64: imageB64 },
  )
}

/** 生成定妆图（POST …/characters/:cid/portrait/generate → 202 任务；prompt
 *  缺省由角色 description 构造）。 */
export function filmGenPortrait(
  projectId: string,
  characterId: string,
  modelRef: FilmModelRef,
  prompt?: string,
): Promise<FilmTask> {
  return api().post<FilmTask>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/characters/${encodeURIComponent(characterId)}/portrait/generate`,
    { model_ref: modelRef, ...(prompt ? { prompt } : {}) },
  )
}
/** 导入项目参考图（POST /api/v1/film/projects/:id/refs；b64 png/jpeg/webp）。 */
export function filmUploadRef(
  projectId: string,
  imageB64: string,
  filename?: string,
): Promise<{ name: string; path: string; bytes: number; filename?: string | null }> {
  return api().post(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/refs`,
    filename ? { image_b64: imageB64, filename } : { image_b64: imageB64 },
  )
}

/**
 * 经既有产物读取路径（GET /api/v1/files/download?path=，b64 信封）取文件字节
 * 并转 data URL——film 产物不经 apps-assets，定妆图/参考图缩略共用此helper。
 */
export async function fetchFileDataUrl(pathOrUrl: string): Promise<string> {
  const path = pathOrUrl.startsWith('/api/v1/files/download')
    ? pathOrUrl
    : `/api/v1/files/download?path=${encodeURIComponent(pathOrUrl)}`
  const env = await api().get<{ mime_type?: string; content_base64?: string }>(path)
  const mime = env.mime_type || 'image/png'
  return `data:${mime};base64,${env.content_base64 ?? ''}`
}

/** File → data URL（定妆图/参考图上传前读文件用）。 */
export function readFileAsDataUrl(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader()
    reader.onload = () => resolve(String(reader.result))
    reader.onerror = () => reject(reader.error ?? new Error('读取文件失败'))
    reader.readAsDataURL(file)
  })
}

/** data URL → {b64, mime}（剥离 data: 前缀；后端要求原始标准 b64）。 */
export function splitDataUrl(dataUrl: string): { b64: string; mime: string } {
  const m = /^data:([^;,]+)?;base64,(.*)$/s.exec(dataUrl)
  if (!m) return { b64: dataUrl, mime: '' }
  return { mime: m[1] ?? '', b64: m[2] ?? '' }
}

// =============================================================================
// FilmHub 流程化契约（v0.1.35 冻结；后端并行开发中——接口失败如实展示不崩）。
//
// 流程：建项目 → 剧情页（story/import + story/generate）→ 分镜页
// （storyboard/generate）→ 定妆页（casting/extract + casting/:type CRUD +
// views generate/import）→ 音频页（audio/bgm 库）→ 合成（compose +
// dist/final-v*.mp4 版本列表 + cache 半成品 commit 转正）。
//
// 通用面：files 树（GET/PUT :id/files/<path>——ownership.json / activity.json
// 等协作文件走此）+ cost 聚合 + export/import。
//
// 多人分工 v1：写类端点 body 可带 author（缺省 "anonymous"，后端落 activity
// 流水 [{ts,author,action,target}] 环形 200 条）。
// =============================================================================

/** 定妆六类（casting/:type 路径段）。 */
export const FILM_CAST_TYPES = [
  'characters',
  'props',
  'pets',
  'formations',
  'actions',
  'scenes',
] as const
export type FilmCastType = (typeof FILM_CAST_TYPES)[number]

/** 定妆对象（GET/POST /film/projects/:id/casting/:type 元素；宽松字段）。 */
export interface FilmCastingObject {
  name: string
  desc?: string | null
  /** TTS 音色（人物类；透传配音）。 */
  voice?: string | null
  /** 定制器部件选中值（v0.1.39；card.md front-matter parts 回传）。 */
  parts?: Record<string, string> | null
  /** 动作参考子选项（v0.1.41 P0；仅 actions 类；card.md motion 回传）。 */
  motion?: { kind: 'video' | 'skeleton'; asset: string; note?: string } | null
  /** 运镜子选项（v0.1.41 P0；仅 actions 类；card.md camera 回传）。 */
  camera?: { kind: 'enum' | 'describe'; preset?: string; describe?: string } | null
  /** 多视图清单（后端回传；view ∈ 五槽位 + 主槽 -vN 版本）。 */
  views?: { view?: string; path?: string | null; url?: string | null; bytes?: number | null }[] | null
  [k: string]: unknown
}

/** 提取报告条目（extraction.json 六类分组元素；宽松字段）。 */
export interface FilmExtractionItem {
  name?: string
  desc?: string
  frequency?: number
  [k: string]: unknown
}

/** 提取报告（casting/extract 产物 extraction.json；六类分组）。 */
export interface FilmCastingExtraction {
  characters?: FilmExtractionItem[]
  props?: FilmExtractionItem[]
  pets?: FilmExtractionItem[]
  formations?: FilmExtractionItem[]
  actions?: FilmExtractionItem[]
  scenes?: FilmExtractionItem[]
  [k: string]: unknown
}

/** BGM 库条目（GET/POST :id/audio/bgm；宽松字段——track/name/id 均可为键）。 */
export interface FilmBgmEntry {
  track?: string
  name?: string
  id?: string
  /** trigger/mood 平铺或 info 嵌套两形态兼容。 */
  info?: { trigger?: string; mood?: string } | null
  trigger?: string
  mood?: string
  /** 音频文件（非空=已有 track）。 */
  file?: string | null
  path?: string | null
  bytes?: number | null
  duration_secs?: number | null
  [k: string]: unknown
}

/** BGM 条目标识（track > name > id；调用 :track 路径段用）。 */
export function bgmEntryKey(e: FilmBgmEntry): string {
  return e.track ?? e.name ?? e.id ?? ''
}

/** BGM 条目的 trigger / mood（平铺与 info 嵌套两形态归一）。 */
export function bgmEntryTrigger(e: FilmBgmEntry): string {
  return (e.trigger ?? e.info?.trigger ?? '').trim()
}
export function bgmEntryMood(e: FilmBgmEntry): string {
  return (e.mood ?? e.info?.mood ?? '').trim()
}

/** 项目文件树条目（GET :id/files；path 相对项目目录，含目录前缀）。 */
export interface FilmFileEntry {
  path: string
  bytes?: number
  kind?: string
  mtime?: string | null
  modified_at?: string | null
  [k: string]: unknown
}

/** 单文件读响应（GET :id/files/<path>；b64 信封）。 */
export interface FilmFileContent {
  content_b64?: string
  /** hub files_get 文本直回字段（kind=text 时原文；v0.1.37 归一见 filmContentText）。 */
  content?: string
  /** hub files_get 二进制 b64 信封字段。 */
  content_base64?: string
  mime?: string
  mime_type?: string
  [k: string]: unknown
}

/** 成本聚合响应（GET :id/cost?by=stage|channel|day；宽松字段）。 */
export interface FilmCostGroup {
  key?: string
  stage?: string
  channel?: string
  cost?: number
  est_cost?: number
  events?: number
  calls?: number
  [k: string]: unknown
}
export interface FilmCostReport {
  total?: number
  currency?: string
  calls?: number
  events?: number
  groups?: FilmCostGroup[]
  [k: string]: unknown
}

/** b64 → UTF-8 文本（files 信封内容解码；TextDecoder 走字节防中文乱码）。 */
export function b64ToText(b64: string): string {
  if (!b64) return ''
  const bin = atob(b64)
  const bytes = new Uint8Array(bin.length)
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i)
  return new TextDecoder().decode(bytes)
}

/** 项目内相对路径 → URL（按段编码，保留 / 分隔）。 */
function encodeFilePath(path: string): string {
  return path
    .split('/')
    .filter(Boolean)
    .map((seg) => encodeURIComponent(seg))
    .join('/')
}

/** 任务判定（casting/extract 等端点 202 任务 / 200 直出两形态归一）。 */
export function isFilmTask(x: unknown): x is FilmTask {
  return (
    !!x &&
    typeof x === 'object' &&
    typeof (x as { id?: unknown }).id === 'string' &&
    typeof (x as { status?: unknown }).status === 'string'
  )
}

// —— 剧情页 ——

/** 导入剧情原文（POST :id/story/import {filename, content_b64, author?}）。
 *  后端只收 UTF-8（非 UTF-8 字节 400「导入原文须为 UTF-8 文本」）——
 *  content_b64 由前端保证 UTF-8：StoryPage 导入处先 fatal UTF-8 严格校验，
 *  GBK/GB2312/GB18030 原文自动转码后再上传（v0.1.39，见 StoryPage
 *  decodeSourceBytes）。 */
export function filmStoryImport(
  projectId: string,
  body: { filename: string; content_b64: string; author?: string },
): Promise<{ name?: string; filename?: string; bytes?: number; [k: string]: unknown }> {
  return api().post(`/api/v1/film/projects/${encodeURIComponent(projectId)}/story/import`, body)
}

/** AI 写剧情（POST :id/story/generate {model_ref?, prompt?, source_file?, author?}）
 *  → 任务（stage=story；产物 story.md）。model_ref 缺省读项目 chat 默认。 */
export function filmStoryGenerate(
  projectId: string,
  body: { model_ref?: FilmModelRef; prompt?: string; source_file?: string; author?: string },
): Promise<FilmTask> {
  return api().post<FilmTask>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/story/generate`,
    body,
  )
}

// —— 剧情文档处理管线（v0.1.37：清理 → 分章 → 人物梳理） ——

/** 清理方式：rules=本地规则零成本；llm=规则先行 + 分块深清（须 model_ref）。 */
export type FilmStoryCleanMode = 'rules' | 'llm'

/** 原文清理（POST :id/story/clean → 202 任务 stage=story.clean；
 *  产物 story/cleaned-<源>.txt + .report.json）。 */
export function filmStoryClean(
  projectId: string,
  body: {
    source_file: string
    mode?: FilmStoryCleanMode
    model_ref?: FilmModelRef
    author?: string
  },
): Promise<FilmTask> {
  return api().post<FilmTask>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/story/clean`,
    body,
  )
}

/** 章节划分（POST :id/story/chapterize → 202 任务 stage=story.chapterize；
 *  cleaned 优先；无章节结构自动分段。产物 story/chapters/index.json + ch-<NN>.md。 */
export function filmStoryChapterize(
  projectId: string,
  body: { model_ref?: FilmModelRef; source_file?: string; author?: string },
): Promise<FilmTask> {
  return api().post<FilmTask>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/story/chapterize`,
    body,
  )
}

/** 人物梳理（POST :id/story/profile → 202 任务 stage=story.profile；
 *  分块提取 + 别名归并。产物 story/characters-profile.json）。 */
export function filmStoryProfile(
  projectId: string,
  body: {
    model_ref?: FilmModelRef
    source_file?: string
    chapter_range?: string
    author?: string
  },
): Promise<FilmTask> {
  return api().post<FilmTask>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/story/profile`,
    body,
  )
}

// —— 语义检索（v0.1.39.1：emb 能力位消费；hub 树 vectors-*.json 即缓存） ——

/**
 * 源文向量化（POST :id/story/embed {model_ref?, source_file?, author?} →
 * 202 任务 stage=story.embed）。~800 字段落块逐块走 embeddings 端点
 * （local vLLM /v1/embeddings——chat 实例无该端点 404 如实报错；channel
 * {base_url}/embeddings）；产物 story/vectors-<源基名>.json 落树即缓存
 * （重复 embed 覆盖）。model_ref 缺省读项目 emb 位。
 */
export function filmStoryEmbed(
  projectId: string,
  body: { model_ref?: FilmModelRef; source_file?: string; author?: string },
): Promise<FilmTask> {
  return api().post<FilmTask>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/story/embed`,
    body,
  )
}

/** 语义检索结果条目（POST :id/story/search 的 results 元素）。 */
export interface FilmStorySearchHit {
  i: number
  text: string
  score: number
  start_line?: number
  [k: string]: unknown
}

/** 语义检索响应（POST :id/story/search {query, model_ref?, source_file?, top_k?}）。 */
export interface FilmStorySearchResult {
  query?: string
  source?: string
  vectors?: string
  model?: string
  top_k?: number
  results?: FilmStorySearchHit[]
  [k: string]: unknown
}

/** 语义检索（同步 200：query 向量化 → 与向量缓存 chunks 余弦扫 → top_k）。 */
export function filmStorySearch(
  projectId: string,
  body: { query: string; model_ref?: FilmModelRef; source_file?: string; top_k?: number },
): Promise<FilmStorySearchResult> {
  return api().post<FilmStorySearchResult>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/story/search`,
    body,
  )
}

/** 章节清单条目（story/chapters/index.json 的 chapters 元素）。 */
export interface FilmChapterEntry {
  no: number
  title: string
  start_line?: number
  end_line?: number
  words?: number
  auto?: boolean
  file?: string
  [k: string]: unknown
}

/** 章节清单（story/chapters/index.json）。 */
export interface FilmChapterIndex {
  version?: number
  source?: string
  auto?: boolean
  chapters: FilmChapterEntry[]
  [k: string]: unknown
}

/** 人物档案条目（story/characters-profile.json 根数组元素）。 */
export interface FilmCharacterProfile {
  name: string
  aliases?: string[]
  gender?: string
  age?: string
  appearance?: string
  personality?: string
  relations?: { name?: string; relation?: string }[]
  first_chapter?: number | null
  arc?: string
  [k: string]: unknown
}

// —— 分镜页 ——

/** 从剧情生成分镜（POST :id/storyboard/generate {model_ref?, author?, …配置?}）
 *  → 任务（stage=storyboard；覆盖现有分镜——前端先确认）。model_ref 缺省读项目
 *  默认。opts 为生成分镜配置面板拼装的可选字段（后端 v1 未消费——见
 *  FilmStoryboardGenOptions）。 */
export function filmStoryboardGenerate(
  projectId: string,
  modelRef?: FilmModelRef,
  author?: string,
  opts?: FilmStoryboardGenOptions,
): Promise<FilmTask> {
  return api().post<FilmTask>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/storyboard/generate`,
    {
      ...(modelRef ? { model_ref: modelRef } : {}),
      ...(author ? { author } : {}),
      ...storyboardGenBodyFields(opts),
    },
  )
}

// —— 定妆页 ——

/** AI 提取定妆对象（POST :id/casting/extract {model_ref?, author?}）→ 202 任务
 *  （stage=casting）或 200 直出报告（产物 extraction.json 六类）。 */
export function filmCastingExtract(
  projectId: string,
  modelRef?: FilmModelRef,
  author?: string,
): Promise<FilmTask | FilmCastingExtraction> {
  return api().post(`/api/v1/film/projects/${encodeURIComponent(projectId)}/casting/extract`, {
    ...(modelRef ? { model_ref: modelRef } : {}),
    ...(author ? { author } : {}),
  })
}

/** 定妆对象列表（GET :id/casting/:type；type=六类）。 */
export function filmListCasting(
  projectId: string,
  type: FilmCastType,
): Promise<FilmCastingObject[]> {
  return api().get<FilmCastingObject[]>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/casting/${encodeURIComponent(type)}`,
  )
}

/** 新建定妆对象（POST :id/casting/:type {name, desc, voice?, author?}）。 */
export function filmCreateCasting(
  projectId: string,
  type: FilmCastType,
  body: { name: string; desc: string; voice?: string; author?: string },
): Promise<FilmCastingObject> {
  return api().post<FilmCastingObject>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/casting/${encodeURIComponent(type)}`,
    body,
  )
}

/** 更新定妆对象（PUT :id/casting/:type/:name {desc?, voice?, parts?, author?,
 * motion?, camera?}；parts 整表替换——v0.1.39 定制器部件选中值；motion/camera
 * =v0.1.41 P0 动作参考/运镜子选项（仅 actions 类；null=清空，缺省=不动））。 */
export function filmUpdateCasting(
  projectId: string,
  type: FilmCastType,
  name: string,
  body: {
    desc?: string
    voice?: string
    parts?: Record<string, string>
    motion?: { kind: 'video' | 'skeleton'; asset: string; note?: string } | null
    camera?: { kind: 'enum' | 'describe'; preset?: string; describe?: string } | null
    author?: string
  },
): Promise<FilmCastingObject> {
  return api().request<FilmCastingObject>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/casting/${encodeURIComponent(type)}/${encodeURIComponent(name)}`,
    { method: 'PUT', body },
  )
}

/** 删除定妆对象（DELETE :id/casting/:type/:name）。 */
export function filmDeleteCasting(
  projectId: string,
  type: FilmCastType,
  name: string,
  author?: string,
): Promise<unknown> {
  return api().request(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/casting/${encodeURIComponent(type)}/${encodeURIComponent(name)}`,
    { method: 'DELETE', body: author ? { author } : undefined },
  )
}

/** AI 生成对象视图（POST :id/casting/:type/:name/views/generate
 *  {model_ref?, view, prompt?, author?, source?}）→ 任务。model_ref 缺省读项目
 *  image 默认；source=blender（v0.1.44 spike）走 chat 默认 + view 可省=render。 */
export function filmGenCastView(
  projectId: string,
  type: FilmCastType,
  name: string,
  body: {
    model_ref?: FilmModelRef
    view?: string
    prompt?: string
    author?: string
    source?: 'image' | 'blender'
  },
): Promise<FilmTask> {
  return api().post<FilmTask>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/casting/${encodeURIComponent(type)}/${encodeURIComponent(name)}/views/generate`,
    body,
  )
}

/** 导入对象视图（POST :id/casting/:type/:name/views/import
 *  {image_b64|video_b64, view, mime?, author?}；图片 png/jpeg/webp ≤10MB，
 *  视频 mp4（v0.1.41 P0：mime=video/mp4 + ftyp 魔数）≤50MB
 *  （env NEXOS_FILM_ASSET_MAX_MB 可调））。 */
export function filmImportCastView(
  projectId: string,
  type: FilmCastType,
  name: string,
  body: {
    image_b64?: string
    video_b64?: string
    view: string
    mime?: string
    author?: string
  },
): Promise<FilmCastingObject> {
  return api().post<FilmCastingObject>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/casting/${encodeURIComponent(type)}/${encodeURIComponent(name)}/views/import`,
    body,
  )
}

// —— 音频页（BGM 库）——

/** BGM 库列表（GET :id/audio/bgm）。 */
export function filmListBgm(projectId: string): Promise<FilmBgmEntry[]> {
  return api().get<FilmBgmEntry[]>(`/api/v1/film/projects/${encodeURIComponent(projectId)}/audio/bgm`)
}

/** 新建 BGM 条目（POST :id/audio/bgm {info:{trigger,mood}, track_b64?, author?}）。 */
export function filmCreateBgm(
  projectId: string,
  body: { info: { trigger: string; mood?: string }; track_b64?: string; author?: string },
): Promise<FilmBgmEntry> {
  return api().post<FilmBgmEntry>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/audio/bgm`,
    body,
  )
}

/** 删除 BGM 条目（DELETE :id/audio/bgm/:track）。 */
export function filmDeleteBgm(projectId: string, track: string, author?: string): Promise<unknown> {
  return api().request(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/audio/bgm/${encodeURIComponent(track)}`,
    { method: 'DELETE', body: author ? { author } : undefined },
  )
}

/** AI 生成 BGM 音频（POST :id/audio/bgm/:track/generate {model_ref?, prompt?,
 *  author?}）。model_ref 缺省读项目 music 默认。 */
export function filmGenBgm(
  projectId: string,
  track: string,
  body: { model_ref?: FilmModelRef; prompt?: string; author?: string },
): Promise<FilmTask> {
  return api().post<FilmTask>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/audio/bgm/${encodeURIComponent(track)}/generate`,
    body,
  )
}

// —— cache 半成品转正 ——

/** cache 半成品转正（POST :id/cache/:file/commit {author?}；file=cache/ 下文件名）。 */
export function filmCacheCommit(
  projectId: string,
  file: string,
  author?: string,
): Promise<{ committed?: string; [k: string]: unknown }> {
  return api().post(`/api/v1/film/projects/${encodeURIComponent(projectId)}/cache/${encodeURIComponent(file)}/commit`, {
    ...(author ? { author } : {}),
  })
}

// —— 通用 files 面（树 / 单文件读写；ownership.json / activity.json 走此）——

/** 项目文件树（GET :id/files → [{path, bytes, kind, mtime?}]）。 */
export function filmListFiles(projectId: string): Promise<FilmFileEntry[]> {
  return api().get<FilmFileEntry[]>(`/api/v1/film/projects/${encodeURIComponent(projectId)}/files`)
}

/** 读项目内单文件（GET :id/files/<path> → 文本 {content} / 二进制 b64 信封）。 */
export function filmGetFile(projectId: string, path: string): Promise<FilmFileContent> {
  return api().get<FilmFileContent>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/files/${encodeFilePath(path)}`,
  )
}

/** files 读信封 → 文本（两契约归一：hub 文本直回 `content` 字符串；b64 信封
 *  content_b64 / 二进制 content_base64 解码——剧情页/章节/档案读取共用）。 */
export function filmContentText(env: FilmFileContent): string {
  if (typeof env.content === 'string') return env.content
  if (typeof env.content_b64 === 'string' && env.content_b64) return b64ToText(env.content_b64)
  if (typeof env.content_base64 === 'string' && env.content_base64) {
    return b64ToText(env.content_base64)
  }
  return ''
}

/** 写项目内单文件（PUT :id/files/<path> {content_b64, author?}）。 */
export function filmPutFile(
  projectId: string,
  path: string,
  contentB64: string,
  author?: string,
): Promise<{ written?: boolean; bytes?: number; [k: string]: unknown }> {
  return api().request(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/files/${encodeFilePath(path)}`,
    { method: 'PUT', body: { content_b64: contentB64, ...(author ? { author } : {}) } },
  )
}

/** 项目内文件 → data URL（图/音预览用；经 files b64 信封）。 */
export async function filmFileDataUrl(projectId: string, path: string): Promise<string> {
  const env = await filmGetFile(projectId, path)
  return `data:${env.mime || env.mime_type || 'application/octet-stream'};base64,${env.content_b64 ?? ''}`
}

// —— 成本聚合 ——

/** 成本聚合（GET :id/cost?by=stage|channel|day；徽章用无参聚合，面板 by 细分）。 */
export function filmGetCost(
  projectId: string,
  by?: 'stage' | 'channel' | 'day',
): Promise<FilmCostReport> {
  const q = by ? `?by=${encodeURIComponent(by)}` : ''
  return api().get<FilmCostReport>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/cost${q}`,
  )
}

// —— 导出 / 导入（文件 ⇄ 项目）——

/** 导出项目为文件树（POST :id/export {target?, author?} → 202 任务）。 */
export function filmExportProject(
  projectId: string,
  target?: 'files' | 'export_dir',
  author?: string,
): Promise<FilmTask> {
  return api().post<FilmTask>(`/api/v1/film/projects/${encodeURIComponent(projectId)}/export`, {
    ...(target ? { target } : {}),
    ...(author ? { author } : {}),
  })
}

/** 从文件树导入建项目（POST /film/projects/import {dir, author?} → 201 项目）。 */
export function filmImportProject(dir: string, author?: string): Promise<FilmProject> {
  return api().post<FilmProject>('/api/v1/film/projects/import', {
    dir,
    ...(author ? { author } : {}),
  })
}

// —— PR/Issues 协作层（v0.1.10：hub/collab/issues.json · prs.json 文件即真值；
//    REST 便捷面 + files GET 直读双通道；PR 的 branch→base 映射项目 git 仓） ——

/** Issue/PR 评论条目（环形 200）。 */
export interface FilmCollabComment {
  author: string
  body: string
  created_at: string
}

/** Issue 状态（PUT 三态）。 */
export type FilmIssueState = 'open' | 'in-progress' | 'closed'

/** Issue（issues.json 元素；宽松字段）。 */
export interface FilmIssue {
  iid: number
  title: string
  body?: string
  state: FilmIssueState
  labels?: string[]
  assignee?: string | null
  author: string
  /** 管线环节名（环节错误归因；from-task 自动填）。 */
  stage?: string | null
  created_at: string
  closed_at?: string | null
  comments?: FilmCollabComment[]
  [k: string]: unknown
}

/** PR 状态。 */
export type FilmPrState = 'open' | 'merged' | 'closed'

/** PR（prs.json 元素）。 */
export interface FilmPr {
  pid: number
  title: string
  body?: string
  /** head 分支 → 目标基线（项目 git 仓映射）。 */
  branch: string
  base: string
  state: FilmPrState
  /** 合并时自动 close 的 issue iid 列表。 */
  issue_refs?: number[]
  author: string
  created_at: string
  merged_at?: string | null
  merged_by?: string | null
  merge_commit?: string | null
  closed_at?: string | null
  comments?: FilmCollabComment[]
  [k: string]: unknown
}

/** Issue 列表响应（issues 新→旧 + counts；open 计数含 in-progress）。 */
export interface FilmIssueList {
  state?: string
  issues: FilmIssue[]
  counts?: { open?: number; in_progress?: number; closed?: number }
  [k: string]: unknown
}

/** PR 列表响应。 */
export interface FilmPrList {
  state?: string
  prs: FilmPr[]
  counts?: { open?: number; merged?: number; closed?: number }
  [k: string]: unknown
}

/** PR diff 响应（git diff base...head 文本；超 256KB 截断标注）。 */
export interface FilmPrDiff {
  pid?: number
  base?: string
  head?: string
  diff?: string
  truncated?: boolean
  [k: string]: unknown
}

/** Issue 列表（GET :id/collab/issues?state=open|closed|all；缺省 open）。 */
export function filmListIssues(
  projectId: string,
  state?: 'open' | 'closed' | 'all',
): Promise<FilmIssueList> {
  const q = state ? `?state=${encodeURIComponent(state)}` : ''
  return api().get<FilmIssueList>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/collab/issues${q}`,
  )
}

/** Issue 详情（含评论）。 */
export function filmGetIssue(projectId: string, iid: number): Promise<FilmIssue> {
  return api().get<FilmIssue>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/collab/issues/${iid}`,
  )
}

/** 新建 Issue（{title, body?, labels?, assignee?, author, stage?}；stage=管线
 *  环节名——环节错误归因标签）。 */
export function filmCreateIssue(
  projectId: string,
  body: {
    title: string
    body?: string
    labels?: string[]
    assignee?: string
    author?: string
    stage?: string
  },
): Promise<FilmIssue> {
  return api().post<FilmIssue>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/collab/issues`,
    body,
  )
}

/** 更新 Issue（{title?/body?/state?/assignee?/labels?}；state 三态——closed 记
 *  closed_at，重开清之）。 */
export function filmUpdateIssue(
  projectId: string,
  iid: number,
  body: {
    title?: string
    body?: string
    state?: FilmIssueState
    assignee?: string | null
    labels?: string[]
    author?: string
  },
): Promise<FilmIssue> {
  return api().request<FilmIssue>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/collab/issues/${iid}`,
    { method: 'PUT', body },
  )
}

/** Issue 评论（环形 200）。 */
export function filmCommentIssue(
  projectId: string,
  iid: number,
  body: { author?: string; body: string },
): Promise<FilmCollabComment> {
  return api().post<FilmCollabComment>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/collab/issues/${iid}/comments`,
    body,
  )
}

/** 环节错误一键提 Issue（读任务 stage/error/日志尾/源文件自动成文；
 *  title=[环节] <stage> 失败 + labels=[pipeline-error]）。 */
export function filmIssueFromTask(
  projectId: string,
  body: { task_id: string; author?: string; note?: string },
): Promise<{ issue?: FilmIssue; task_id?: string; from_task?: boolean }> {
  return api().post(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/collab/issues/from-task`,
    body,
  )
}

/** PR 列表（GET :id/collab/prs?state=open|merged|closed|all；缺省 open）。 */
export function filmListPrs(
  projectId: string,
  state?: 'open' | 'merged' | 'closed' | 'all',
): Promise<FilmPrList> {
  const q = state ? `?state=${encodeURIComponent(state)}` : ''
  return api().get<FilmPrList>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/collab/prs${q}`,
  )
}

/** PR 详情（含评论）。 */
export function filmGetPr(projectId: string, pid: number): Promise<FilmPr> {
  return api().get<FilmPr>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/collab/prs/${pid}`,
  )
}

/** PR diff（git diff base...head 文本）。 */
export function filmGetPrDiff(projectId: string, pid: number): Promise<FilmPrDiff> {
  return api().get<FilmPrDiff>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/collab/prs/${pid}/diff`,
  )
}

/** 新建 PR（{title, branch, base?=main, issue_refs?[], author, body?}；branch
 *  须为项目 git 仓已存在分支——下拉数据源 filmGitBranches）。 */
export function filmCreatePr(
  projectId: string,
  body: {
    title: string
    branch: string
    base?: string
    issue_refs?: number[]
    author?: string
    body?: string
  },
): Promise<FilmPr> {
  return api().post<FilmPr>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/collab/prs`,
    body,
  )
}

/** 更新 PR（{title?/state?}；state 仅可 closed——open→merged 走 merge）。 */
export function filmUpdatePr(
  projectId: string,
  pid: number,
  body: { title?: string; state?: 'closed'; author?: string },
): Promise<FilmPr> {
  return api().request<FilmPr>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/collab/prs/${pid}`,
    { method: 'PUT', body },
  )
}

/** PR 评论。 */
export function filmCommentPr(
  projectId: string,
  pid: number,
  body: { author?: string; body: string },
): Promise<FilmCollabComment> {
  return api().post<FilmCollabComment>(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/collab/prs/${pid}/comments`,
    body,
  )
}

/** PR 合并（checkout base → merge --no-ff → 关联 issues 自动 close；返回
 *  merge_commit 全 hash + closed_issues；冲突 409 / 脏树 400）。 */
export function filmMergePr(
  projectId: string,
  pid: number,
  body: { author?: string },
): Promise<{
  merged?: number
  base?: string
  branch?: string
  merge_commit?: string
  head?: string | null
  closed_issues?: number[]
  merged_by?: string
}> {
  return api().post(
    `/api/v1/film/projects/${encodeURIComponent(projectId)}/collab/prs/${pid}/merge`,
    body,
  )
}

// —— 项目 git 仓（v0.1.42；PR 新建分支下拉数据源） ——

/** git 分支条目。 */
export interface FilmGitBranch {
  name: string
  hash?: string
  time?: string
  message?: string
  [k: string]: unknown
}

/** 分支清单（GET :id/git/branches → {current, branches}）。 */
export function filmGitBranches(
  projectId: string,
): Promise<{ current?: string; branches?: FilmGitBranch[] }> {
  return api().get(`/api/v1/film/projects/${encodeURIComponent(projectId)}/git/branches`)
}
