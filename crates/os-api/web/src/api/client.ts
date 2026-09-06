// =============================================================================
// OS System REST API 客户端（TypeScript 版，对齐 static/js/api.js）
//
// 全部同源（baseUrl 为空），由 os-api 网关直接提供静态资源 + API。
// 提供：
//   - request(path, opts?)  统一 fetch + 超时 + 错误抛出
//   - get / post / del      语义化封装
//   - endpoints             25 路由的具体调用（强类型）
// =============================================================================
import type {
  Capacity,
  CreatePoolRequest,
  CreateVmRequest,
  Dataset,
  DiskInfo,
  DiskPartitions,
  ImGroup,
  ImLobbyInfo,
  ImLobbyMember,
  ImMessage,
  ImPeer,
  InitializeDiskResp,
  NfsExport,
  NodeInfo,
  Pool,
  Share,
  Snapshot,
  SystemStatus,
  User,
  VersionInfo,
  Vm,
} from './types';

/** 同源调用基址（网关在同一 origin 暴露静态资源与 API）。 */
const BASE_URL = '';

/** 统一请求超时（毫秒）。 */
const TIMEOUT_MS = 15_000;

/** API 错误（携带 HTTP status + path + message，便于 UI 展示）。 */
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

// —— 可选全局 API Token（服务端启用 NEXOS_ADMIN_TOKEN 时，用于发布/克隆等写操作鉴权）——

/** localStorage key（token 仅存本浏览器）。 */
const TOKEN_STORAGE_KEY = 'os-api-token';

/** localStorage 不可用（隐私模式等）时的内存降级变量。 */
let memToken = '';

/** 读取全局 token（不存在返回空串；localStorage 抛异常时降级内存变量）。 */
export function getApiToken(): string {
  try {
    return window.localStorage.getItem(TOKEN_STORAGE_KEY) ?? '';
  } catch {
    return memToken;
  }
}

/** 保存/清除全局 token（传空串即清除）。localStorage 写失败时仅存内存变量。 */
export function setApiToken(v: string): void {
  const t = (v ?? '').trim();
  memToken = t;
  try {
    if (t) window.localStorage.setItem(TOKEN_STORAGE_KEY, t);
    else window.localStorage.removeItem(TOKEN_STORAGE_KEY);
  } catch {
    /* 隐私模式：降级为内存变量（刷新后失效） */
  }
}

type FetchOpts = {
  method?: string;
  headers?: Record<string, string>;
  body?: unknown;
  signal?: AbortSignal;
};

/** 统一 fetch 封装：返回 JSON；非 2xx 抛 ApiError；支持超时（AbortController）。 */
export async function request<T>(path: string, opts: FetchOpts = {}): Promise<T> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), TIMEOUT_MS);

  try {
    const headers: Record<string, string> = { Accept: 'application/json' };
    // 全局 token 存在且**非空**时注入 Authorization 头。空串/未配置一律不带头
    //——服务端测试期对"无凭据请求"注入默认 admin（NEXOS_AUTH_DEFAULT_ADMIN），
    // 带了过期错误的头反而会被真实验证拒绝（401）。用户在设置里显式填 token 后
    // 照常带头。
    const token = getApiToken();
    if (typeof token === 'string' && token.trim()) {
      headers['Authorization'] = `Bearer ${token}`;
    }
    // 调用方显式传入的 header 最后合入——可覆盖全局注入
    //（IM 用户面端点带 IM token 覆盖系统 admin token，两者同 header 格式但值不同）
    if (opts.headers) Object.assign(headers, opts.headers);
    const init: RequestInit = {
      method: opts.method ?? 'GET',
      headers,
      signal: opts.signal ?? controller.signal,
    };
    if (opts.body !== undefined) {
      (init as RequestInit).body = JSON.stringify(opts.body);
      headers['Content-Type'] = 'application/json';
    }

    const resp = await fetch(BASE_URL + path, init);
    if (!resp.ok) {
      // 401 统一为友好提示（服务端启用 NEXOS_ADMIN_TOKEN 且未配置/填错 token 时出现）
      if (resp.status === 401) {
        throw new ApiError('未授权（401）——请在 设置 → API 令牌 填写管理员 token', {
          status: resp.status,
          path,
        });
      }
      // 尝试从响应体提取错误信息
      let detail = '';
      try {
        const body = await resp.json();
        detail = (body && (body.error || body.message)) || JSON.stringify(body);
      } catch {
        try {
          detail = await resp.text();
        } catch {
          /* ignore */
        }
      }
      throw new ApiError(
        `${resp.status} ${resp.statusText}${detail ? ' — ' + detail : ''}`,
        { status: resp.status, path },
      );
    }
    // 某些端点（204/创建）可能返回空体，安全降级为 null
    const text = await resp.text();
    return (text ? JSON.parse(text) : null) as T;
  } finally {
    clearTimeout(timer);
  }
}

// —— 语义化简写 ——
export function get<T>(path: string): Promise<T> {
  return request<T>(path, { method: 'GET' });
}
export function post<T>(path: string, body?: unknown): Promise<T> {
  return request<T>(path, { method: 'POST', body });
}
export function del<T>(path: string): Promise<T> {
  return request<T>(path, { method: 'DELETE' });
}

// —— 系统监控：实时网速（GET /api/v1/monitor/net-rate）——

/** 单接口实时网速（bps = 字节/秒，两次 /proc/net/dev 采样差值；已排除 lo）。 */
export interface NetIfaceRate {
  /** 接口名（如 eth0）。 */
  iface: string;
  /** 下行速率（字节/秒）。 */
  rx_bps: number;
  /** 上行速率（字节/秒）。 */
  tx_bps: number;
}

/** 全部非 lo 接口聚合的总速率（bps = 字节/秒）。 */
export interface NetRateSummary {
  rx_bps: number;
  tx_bps: number;
}

/** 实时网速快照：总速率 + 各接口明细（按接口名排序；首次调用全 0 记基线）。 */
export interface NetRateSnapshot {
  total: NetRateSummary;
  interfaces: NetIfaceRate[];
}

// =============================================================================
// 具体端点封装（对齐 os-api 网关 25 路由 + 静态页 api.js 兼容简写）
// =============================================================================

/** 网关令牌计费模式。 */
export type GatewayBillingMode = 'free' | 'per_token' | 'per_image' | 'credits';

/** 创建网关令牌 body（POST /api/v1/gateway/tokens）。 */
export type GatewayTokenCreateBody = {
  /** 令牌名称。 */
  name?: string;
  /** 配额上限（0=无限；credits 模式下由 initial_credits 决定）。 */
  quota_limit?: number;
  /** 允许模型（空=全部）。 */
  allowed_models?: string[];
  /** 过期时间（留空=永不过期）。 */
  expires_at?: string;
  /** 计费模式，缺省 per_token。 */
  billing_mode?: GatewayBillingMode;
  /** 初始积分（仅 credits 模式，服务端写入 quota_limit）。 */
  initial_credits?: number;
};

/** 创建充值订单 body（POST /api/v1/gateway/payments）。 */
export type GatewayPaymentCreateBody = {
  /** 目标令牌 id。 */
  token_id: string;
  /** 支付币种。 */
  currency: 'usdt' | 'btc' | 'evm';
  /** 充值积分数。 */
  credits: number;
};

// —— 远程转发（forwarding：SSH 隧道 + RDP 转发）——

/** SSH 隧道模式：local（-L 本地转发）/ remote（-R 远程转发）/ dynamic（-D SOCKS 动态）。 */
export type SshTunnelMode = 'local' | 'remote' | 'dynamic';

/**
 * SSH 隧道（GET /api/v1/forwarding/ssh 数组元素）。
 *
 * 密钥认证红线：DTO 无任何密码字段——认证只经 private_key_path 指定的
 * 私钥（None = 服务器默认 ~/.ssh/id_ed25519），请求体出现 password 直接 400。
 */
export interface SshTunnel {
  /** 唯一 id。 */
  id: string;
  /** 名称。 */
  name: string;
  /** SSH 服务器主机名/IP。 */
  ssh_host: string;
  /** SSH 端口（默认 22）。 */
  ssh_port: number;
  /** SSH 用户名。 */
  ssh_user: string;
  /** 私钥路径；空 = 默认 ~/.ssh/id_ed25519（ssh -i，支持 ~ 展开）。 */
  private_key_path?: string | null;
  /** 转发模式 local / remote / dynamic。 */
  mode: SshTunnelMode | string;
  /** 本地绑定地址（127.0.0.1:8080）；remote 模式下当作远端绑定。 */
  local_bind: string;
  /** 转发目标主机（local/remote 必填；dynamic 为空）。 */
  remote_host?: string | null;
  /** 转发目标端口（local/remote 必填；dynamic 为空）。 */
  remote_port?: number | null;
  /** os-api 启动时自动拉起。 */
  autostart: boolean;
  /** stopped / running / failed。 */
  status: 'stopped' | 'running' | 'failed' | string;
  /** 运行中 ssh 子进程 pid（可空）。 */
  pid?: number | null;
  /** 最近一次错误摘要（可空）。 */
  error?: string | null;
  /** 创建时间（RFC3339）。 */
  created_at: string;
  /** 最近一次成功启动时间（可空）。 */
  last_started?: string | null;
}

/** 创建 SSH 隧道 body（POST /api/v1/forwarding/ssh，需 admin）。禁止 password 字段。 */
export type SshTunnelCreateBody = {
  name: string;
  ssh_host: string;
  ssh_user: string;
  /** 缺省 22。 */
  ssh_port?: number;
  /** 缺省服务器 ~/.ssh/id_ed25519。 */
  private_key_path?: string;
  /** 缺省 local。 */
  mode?: SshTunnelMode;
  /** host:port 形式，如 127.0.0.1:8080。 */
  local_bind: string;
  /** local/remote 模式必填；dynamic 模式必须为空。 */
  remote_host?: string;
  /** local/remote 模式必填；dynamic 模式必须为空。 */
  remote_port?: number;
  /** 缺省 false。 */
  autostart?: boolean;
};

/**
 * RDP 转发（GET /api/v1/forwarding/rdp 数组元素）——纯 Rust TCP 代理
 * （0.0.0.0:listen_port → target_host:target_port）+ .rdp 文件生成。
 */
export interface RdpForward {
  /** 唯一 id。 */
  id: string;
  /** 名称。 */
  name: string;
  /** 远端 Windows 主机（RDP 服务器）。 */
  target_host: string;
  /** 远端 RDP 端口（默认 3389）。 */
  target_port: number;
  /** 本机监听端口（0.0.0.0:listen_port → target）。 */
  listen_port: number;
  /** os-api 启动时自动拉起。 */
  autostart: boolean;
  /** running / stopped / error。 */
  status: 'running' | 'stopped' | 'error' | string;
  /** 累计接受的连接数（持久化）。 */
  connections: number;
  /** 最近一次错误摘要（可空）。 */
  error?: string | null;
  /** 创建时间（RFC3339）。 */
  created_at: string;
}

/** 创建 RDP 转发 body（POST /api/v1/forwarding/rdp，需 admin）。 */
export type RdpForwardCreateBody = {
  name: string;
  target_host: string;
  /** 缺省 3389。 */
  target_port?: number;
  listen_port: number;
  /** 缺省 false。 */
  autostart?: boolean;
};

/** 转发统计（GET /api/v1/forwarding/stats 响应体）。 */
export interface ForwardingStats {
  ssh_tunnels_total: number;
  ssh_tunnels_running: number;
  rdp_forwards_total: number;
  rdp_forwards_running: number;
  /** RDP 转发累计连接数（所有转发之和）。 */
  rdp_total_connections: number;
}

// —— IM 区块链认证（挑战-签名，docs/IM_BLOCKCHAIN_AUTH_DESIGN.md）——
//
// IM 用户身份 = secp256k1 压缩公钥（0x + 66 hex）；认证 = challenge→sign→verify
// 三步，签发的 IM token（24h）用于全部 /api/v1/im/* 用户面端点的
// `Authorization: Bearer <IM token>`（与系统 admin token 同 header 格式但值不同，
// 经 opts.headers 覆盖注入）。auth 两端点本身公开（无需任何 token）。

/** POST /api/v1/im/auth/challenge 响应：挑战 nonce（60s 单次有效）+ 派生展示名。 */
export interface ImAuthChallengeResp {
  /** 64 hex 随机挑战（对它的 UTF-8 字节做 SHA-256 后 ECDSA 签名）。 */
  nonce: string;
  /** nonce 剩余有效秒数。 */
  expires_in: number;
  /** 公钥派生 EVM 地址（0x + 40 hex）。 */
  display_name: string;
}

/** POST /api/v1/im/auth/verify 响应：IM token（24h，单点登录）。 */
export interface ImAuthVerifyResp {
  /** 64 hex IM token。 */
  token: string;
  /** token 有效秒数。 */
  expires_in: number;
  /** 认证通过的公钥（回显）。 */
  pubkey: string;
  /** 派生 EVM 地址。 */
  display_name: string;
}

/** IM 端点可选参数：携带 IM token（覆盖全局 admin token 注入）。 */
export interface ImOpts {
  /** /api/v1/im/auth/verify 签发的 IM token；省略时退回全局 token。 */
  imToken?: string;
}

/** 构造 IM token 的 Authorization 覆盖头（无 token 时返回 undefined，走全局注入）。 */
function imAuthHeaders(o?: ImOpts): Record<string, string> | undefined {
  return o?.imToken ? { Authorization: `Bearer ${o.imToken}` } : undefined;
}

// —— IM Agent 可见性 + @提及 + 文档传输（docs/IM_AGENTS_AND_FILES.md）——

/** 消息发送者类别：human（缺省）/ agent（展示层自声明；服务端白名单归一，垃圾值→human）。 */
export type ImSenderKind = 'human' | 'agent';

/** 消息内附件（响应/历史/补拉/WS 帧四处同构；filename/size_bytes 恒为服务端落盘真值，伪造自报被覆盖）。 */
export interface ImMessageAttachment {
  file_id: string;
  filename: string;
  size_bytes: number;
  mime?: string | null;
}

/** POST /api/v1/im/files 201 响应（url 为含上传者自身 IM token 的相对直链，token 24h 有效）。 */
export interface ImFileUploadResp {
  file_id: string;
  url: string;
  /** 净化后展示名。 */
  filename: string;
  size_bytes: number;
  mime?: string | null;
}

/** 发消息 body 扩展（全部可选）：sender_kind 展示层自声明；attachment 按 file_id 核对（未知 file_id 400）。 */
export interface ImSendExtras {
  sender_kind?: ImSenderKind;
  attachment?: { file_id: string; filename?: string; size_bytes?: number; mime?: string };
}

/** ImMessage 叠加批次 3 新字段（REST 响应 / 历史与离线补拉 / WS 帧三处一致透传，缺省兼容老消息）。 */
export interface ImMessageExt extends ImMessage {
  /** "human"|"agent"（缺省 human；仅展示语义，勿作权限依据）。 */
  sender_kind?: ImSenderKind;
  /** 服务端从 content 解析的 @ 名字（去重保序；客户端传了也被覆盖）。 */
  mentions?: string[];
  /** 附件（无附件为 null/缺省）。 */
  attachment?: ImMessageAttachment | null;
}

/** 附件上传超时：≤64MiB 文件 base64 ≈ 85MB JSON，统一 15s 超时不敷慢网络使用，放宽到 5 分钟。 */
const IM_FILE_UPLOAD_TIMEOUT_MS = 300_000;

// —— IM 消息推送通知 webhook（docs/IM_AGENTS_AND_FILES.md §7，2026-08-22）——

/** webhook 订阅事件：lobby=大厅新消息；conversation=会话（含群组）新消息。 */
export type ImNotifyEvent = 'lobby' | 'conversation';

/** webhook 注册记录（POST /im/notify/register 201 体 = GET /im/notify/list 数组元素）。 */
export interface ImWebhookRecord {
  /** webhook id（注销用）。 */
  id: string;
  /** agent 的 HTTP 接收端点（服务端 POST 目标，http/https）。 */
  url: string;
  /** 注册者 pubkey（token 反查；自报值一律被忽略）。 */
  owner_pubkey: string;
  /** 订阅事件（注册缺省 ["lobby","conversation"] 双开）。 */
  events: ImNotifyEvent[];
  /** 绑定单个会话（仅 conversation 事件生效；null=全部会话）。 */
  conversation_id: string | null;
  /** "active" | "disabled"（连败 ≥5 次自动注销）。 */
  status: string;
  /** 连败计数（成功投递清零）。 */
  fail_count: number;
  /** 最近一次投递时刻（RFC3339；从未投递为 null）。 */
  last_fired_at: string | null;
  /** 最近一次投递错误 / 自动注销原因（成功投递清空）。 */
  last_error: string | null;
  created_at: string;
}

// —— IM 联邦接收开关（2026-08-23：暂停/恢复接收其他节点的大厅消息）——

/** GET/POST /api/v1/im/federation 响应：联邦接收开关状态 + 语义说明。 */
export interface ImFederationStatus {
  /** true=接收远程大厅消息（默认）；false=暂停接收（服务端 ingest 入口短路）。 */
  enabled: boolean;
  /** 当前状态说明（关闭只影响接收——本地消息与联邦发送不受影响）。 */
  note: string;
}

// —— IM 大厅开放开关 + 远程大厅互联（2026-08-23，节点发现页「进入 IM」联动）——

/** GET/POST /api/v1/im/lobby/access 响应：大厅开放开关状态 + 语义说明。 */
export interface ImLobbyAccessStatus {
  /** true=允许其他节点只读浏览本机大厅（+远程发言落地）；false=默认（不允许）。 */
  lobby_public: boolean;
  /** 当前状态说明文案。 */
  note: string;
}

// —— IM 点对点直通消息 DM（2026-08-30：不经大厅广播，只有双方可见）——

/** GET/POST /api/v1/im/dm/access 响应：直通消息开放开关状态 + 语义说明。 */
export interface ImDmAccessStatus {
  /** true=允许其他身份向你发直通消息（开发阶段默认允许）；false=拒收（403/跨节点丢弃）。 */
  dm_open: boolean;
  /** 当前状态说明文案。 */
  note: string;
}

/** GET /api/v1/im/conversations 数组元素（im_conversations 行；im.rs Conversation）。 */
export interface ImConversationRecord {
  id: string;
  name: string;
  is_group: boolean;
  created_by: string | null;
  created_at: string;
  /** dm-* 会话成员（双方 pubkey）；群组/普通对话为空数组。 */
  members: string[];
}

/** POST /api/v1/im/dm 响应：消息 + 确定性会话 id + 投递路由。 */
export interface ImDmSendResp {
  message: ImMessage;
  /** 确定性会话 id（dm-<排序后双方 pubkey 的 hash>——双端一致）。 */
  conversation_id: string;
  /** "local"=本节点投递；"p2p"=经 P2P 定向发送到对方节点（落地以对方开关为准）。 */
  route: 'local' | 'p2p';
  /** route=p2p 时的送达说明。 */
  note?: string;
}

/** 远程大厅镜像消息（脱敏 DTO：只有文本与元数据，无 attachment/file_url/read_by）。 */
export interface ImLobbyViewMessage {
  id: string;
  sender_id: string;
  sender_name: string | null;
  content: string;
  msg_type: string;
  created_at: string;
  sender_kind: string;
}

/** GET /api/v1/im/lobby/remote/:node_id 响应：对方开放状态 + 只读消息镜像。 */
export interface ImRemoteLobbyView {
  node_id: string;
  /** true=开放（messages 为最近 20 条镜像）/ false=denied / null=timeout（error 有说明）。 */
  public: boolean | null;
  messages?: ImLobbyViewMessage[];
  /** "denied"（对方未开放）| "timeout"（对方无应答）| null。 */
  error?: string | null;
}

// —— 媒体生成（模型管理「生成」区，handlers/media_gen.rs；写需 admin，读公开）——

/** POST /api/v1/media/image 成功响应（png_base64 可直接喂 data URL 渲染/下载）。 */
export interface MediaImageGenResp {
  /** 生成记录 id（img-N）。 */
  id: string;
  /** PNG 字节 base64（无 data: 前缀）。 */
  png_base64: string;
  width: number;
  height: number;
  /** 生成耗时（毫秒）。 */
  elapsed_ms: number;
  /** 服务端产物路径（/tmp/media-gen/…png）。 */
  file_path: string;
}

/** GET /api/v1/media/image/recent 数组元素（环形 50 条，不含图）。 */
export interface MediaImageRecentItem {
  id: string;
  /** prompt 摘要（前 120 字）。 */
  prompt_summary: string;
  width: number;
  height: number;
  steps: number;
  elapsed_ms: number;
  created_at: string;
}

/** 视频生成任务（生命周期 queued→processing→completed(video_url)|failed(error)）。 */
export interface MediaVideoTask {
  id: string;
  prompt: string;
  duration_secs: number;
  /** external / local。 */
  backend: string;
  /** queued / processing / completed / failed。 */
  status: 'queued' | 'processing' | 'completed' | 'failed' | string;
  video_url?: string | null;
  error?: string | null;
  created_at: string;
}

// —— NexHub 链上认证（挑战-签名，与 IM 同款三步；密钥对可共用，token 独立）——
//
// NexHub 大厅写端点（publish/下架/purchase/clone/bounty 全部）鉴权顺序：
// nexhub 链上 token（服务端反查 pubkey 归因，body 自报身份字段一律忽略覆盖）
// → 无/无效时回落系统 admin token（NEXOS_ADMIN_TOKEN）→ 两者皆非 401。
// 前端经 NexhubOpts.nexhubToken 覆盖全局 admin token 注入（与 ImOpts 同款先例）。

/** POST /api/v1/nexhub/auth/challenge 响应：挑战 nonce（60s 单次有效）+ 派生展示名。 */
export interface NexhubAuthChallengeResp {
  nonce: string;
  /** nonce 剩余有效秒数。 */
  expires_in: number;
  /** 公钥派生 EVM 地址（0x + 40 hex）。 */
  display_name: string;
}

/** POST /api/v1/nexhub/auth/verify 响应：nexhub token（24h）。 */
export interface NexhubAuthVerifyResp {
  token: string;
  expires_in: number;
  pubkey: string;
  display_name: string;
}

/** NexHub 写端点可选参数：携带 nexhub token（覆盖全局 admin token 注入）。 */
export interface NexhubOpts {
  /** /api/v1/nexhub/auth/verify 签发的 nexhub token；省略时走全局注入（admin 回落）。 */
  nexhubToken?: string;
}

/** 构造 nexhub token 的 Authorization 覆盖头（无 token 时返回 undefined，走全局注入）。 */
function nexhubAuthHeaders(o?: NexhubOpts): Record<string, string> | undefined {
  return o?.nexhubToken ? { Authorization: `Bearer ${o.nexhubToken}` } : undefined;
}

// —— NexHub 项目级 Issues / Pull Requests（coderepo 协作层，docs/NEXHUB_ISSUES_PR.md）——
//
// 每个代码仓库的 GitHub 式协作交互：没有更改权限的 agent 用链上身份开 Issue/
// 评论/提 PR（author=token 反查 pubkey，body 自报忽略；owner_kind 标记身份类别），
// merge 仅 admin / 仓库 owner（大厅 publisher 同 pubkey）。读全部公开。

/** 单条 Issue（GET/POST /api/v1/coderepo/repos/:name/issues*）。 */
export interface RepoIssue {
  repo: string;
  /** 每仓库独立自增编号。 */
  number: number;
  title: string;
  body: string;
  /** 作者（0x+66hex pubkey 或 'admin'）。 */
  author: string;
  /** 作者展示名（EVM 地址 / 'admin'）。 */
  author_display: string;
  /** 'pubkey' | 'admin'。 */
  owner_kind: string;
  /** 'open' | 'closed'。 */
  state: string;
  labels: string[];
  comment_count: number;
  created_at: string;
  updated_at: string;
}

/** 单条评论（Issue 与 PR 共用，kind 区分）。 */
export interface RepoComment {
  repo: string;
  /** 'issue' | 'pull'。 */
  kind: string;
  parent_number: number;
  /** 每 (仓库,类别,父编号) 内自增。 */
  number: number;
  author: string;
  author_display: string;
  owner_kind: string;
  body: string;
  created_at: string;
}

/** 单条项目级 PR。 */
export interface RepoPull {
  repo: string;
  number: number;
  title: string;
  body: string;
  /** 来源分支（创建时校验已 push 到裸仓）。 */
  from_branch: string;
  /** 目标分支（缺省=仓库实际默认分支）。 */
  to_branch: string;
  author: string;
  author_display: string;
  owner_kind: string;
  /** 'open' | 'merged' | 'closed'。 */
  state: string;
  merged_by: string;
  merged_at: string;
  comment_count: number;
  created_at: string;
  updated_at: string;
}

/** Issue 列表响应（?state=open|closed|all，默认 open）。 */
export interface RepoIssuesResp {
  repo: string;
  state: string;
  issues: RepoIssue[];
}

/** Issue 详情响应（含评论流）。 */
export interface RepoIssueDetailResp {
  issue: RepoIssue;
  comments: RepoComment[];
}

/** PR 列表响应（?state=open|merged|closed|all）。 */
export interface RepoPullsResp {
  repo: string;
  state: string;
  pulls: RepoPull[];
}

/** PR 详情响应（含评论流 + git diff to..from --stat 摘要）。 */
export interface RepoPullDetailResp {
  pull: RepoPull;
  comments: RepoComment[];
  diff_stat: string;
}

// —— API 大厅（推理服务市场，component api-market，docs/API_MARKET.md）——
//
// 发布者身份=区块链公钥**唯一通道**（与 nexhub-lobby 共享 ChainAuth，token 互通；
// api-market 自身无 auth 端点）：publish/unpublish/heartbeat 必须带 nexhub 链上
// token（Bearer，NexhubOpts 覆盖注入，服务端反查 pubkey 归因），**无 admin 回落**
// ——系统 admin token 在写端点一律 401。list/detail/metrics 公开只读。

/** 挂牌 server_config（本地硬件探测 + body 覆盖：body 字段 > 探测值）。 */
export interface ApiMarketServerConfig {
  /** GPU 型号（首卡=探测 nvidia-smi；向后兼容保留，可覆盖）。 */
  gpu_name?: string;
  /** GPU 显存 MiB（首卡镜像；探测；可覆盖。GB10 统一内存首卡为 null，真值在 gpus[0].unified_vram_mb）。 */
  gpu_vram_mb?: number | null;
  /** GPU 数量（探测=gpus.length；无卡=0——CPU-only 节点可发布）。 */
  gpu_count?: number;
  /** 全部 GPU（逐卡；发布覆盖可带简化形态 {name,vram_mb}×N，index 省略）。
   *  统一内存架构（GB10/Jetson，2026-09-03）：vram_mb=null + unified_memory=true +
   *  unified_vram_mb（/proc/meminfo 池总量 MiB）。 */
  gpus?: Array<{
    index?: number;
    name: string;
    vram_mb?: number | null;
    unified_memory?: boolean;
    unified_vram_mb?: number | null;
  }>;
  /** CPU 型号（探测 /proc/cpuinfo 首个 model name；aarch64 回退 lscpu；可覆盖）。 */
  cpu_model?: string;
  /** CPU 核数（探测 /proc/cpuinfo；可覆盖）。 */
  cpu_cores?: number;
  /** 内存 GiB（探测 /proc/meminfo，保留一位小数；可覆盖）。 */
  ram_gb?: number;
  /** 模型名（硬件探测拿不到，**发布必填**，缺 400）。 */
  model_name?: string;
  /** 上下文长度（可选，vLLM --max-model-len 老字段）。 */
  max_model_len?: number;
  /** 上下文长度（可选，发布端自报别名，2026-09-02 起后端透传；与 max_model_len 并列独立）。 */
  context_len?: number;
  /** 量化方案（可选）。 */
  quantization?: string;
  /** 区域（可选）。 */
  region?: string;
}

/** 挂牌 pricing（单价格字段，模式区分语义；free 不得带价格，带则 400）。 */
export interface ApiMarketPricing {
  /** free / per_token / per_image。 */
  mode: 'free' | 'per_token' | 'per_image';
  /** 每 1k token 单价；**per_image 模式下字段复用=每图单价**；付费必填 >0。 */
  price_per_1k_tokens?: number;
  /** 付费缺省 sats（可选 sats/credits）；free 强制 free。 */
  currency?: string;
  /** 计价备注（可选，如「输入+输出合计」）。 */
  note?: string;
}

/** 挂牌接入信息（access_info 列，2026-08-31）：消费者直连凭据。 */
export interface ApiMarketAccessInfo {
  /** 调用凭据（如网关 sk-os- 令牌）——服务端仅对 publisher 本人/admin 返回明文，
   *  其他身份（匿名/他人）返回脱敏 `<前4>***<后4>`（短 key 全掩 ****）。 */
  api_key?: string;
  /** 鉴权头用法（缺省 `Authorization Bearer`；自定义如 `X-Api-Key: <key>`——
   *  curl 示例按字面拼接，`<key>` 占位替换为 api_key）。 */
  auth_header?: string;
  /** 接入备注（非敏感，恒明文）。 */
  notes?: string;
}

/** POST /api/v1/api-market/publish body（鉴权经 NexhubOpts.nexhubToken）。 */
export interface ApiMarketPublishBody {
  /** 商品名（同 pubkey 同名=刷新，保留 id/created_at/download_count）。 */
  api_name: string;
  /** 消费端点（http/https 校验，OpenAI 兼容直连）。 */
  endpoint_url: string;
  description?: string;
  tags?: string[];
  /** 发布者负载监控端点（可空；供服务端代拉 {metrics:{…}}）。 */
  metrics_url?: string;
  pricing: ApiMarketPricing;
  /** 只填 model_name（必填）即可——硬件字段后端自动探测，也可手动覆盖。 */
  server_config: ApiMarketServerConfig;
  /** 消费者接入凭据（可选；重发布带则更新、缺省保留既有）。 */
  access_info?: ApiMarketAccessInfo;
}

/** POST /api/v1/api-market/:id/heartbeat body（6 键全可选；规范化别名表见文档 §5.4）。 */
export interface ApiMarketHeartbeatBody {
  running_req?: number;
  waiting_req?: number;
  /** 显存/KV 缓存占用百分比。 */
  gpu_cache_usage?: number;
  tokens_per_sec?: number;
  latency_ms?: number;
  /** 总负载百分比。 */
  load_pct?: number;
}

// —— 文件上传/下载（数据面最后一公里；经网关 JSON 通道 base64 传输，
//    通道约束见 crates/os-api/src/handlers/files.rs 模块注释）——

/** POST /api/v1/files/upload 成功响应（重名时 name 为避让后的最终名）。 */
export interface FilesUploadResp {
  /** 落盘后的最终文件名（重名自动 -1/-2 后缀）。 */
  name: string;
  /** 字节数。 */
  size_bytes: number;
  /** 落盘绝对路径。 */
  path: string;
}

/** GET /api/v1/files/download 响应信封（文件字节 base64 装载）。 */
export interface FilesDownloadResp {
  name: string;
  path: string;
  size_bytes: number;
  mime_type: string;
  /** 恒为 "base64"。 */
  encoding: string;
  content_base64: string;
}

/** 上传单文件大小上限（2 GiB，与后端 UPLOAD_MAX_BYTES 一致；超限前后端都拒）。 */
export const FILES_MAX_UPLOAD_BYTES = 2 * 1024 * 1024 * 1024;

/**
 * File/Blob → 纯 base64 字符串（无 data: 前缀）。
 * FileReader.readAsDataURL 产物去掉 `data:...;base64,` 头。
 */
export function fileToBase64(f: File | Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onload = () => {
      const s = r.result as string;
      const idx = s.indexOf(',');
      resolve(idx >= 0 ? s.slice(idx + 1) : s);
    };
    r.onerror = () => reject(r.error ?? new Error('读取文件失败'));
    r.readAsDataURL(f);
  });
}

/** base64 → Uint8Array（atob 逐字节；buffer 恒为 ArrayBuffer，可作 BlobPart）。 */
export function base64ToBytes(b64: string): Uint8Array<ArrayBuffer> {
  const bin = atob(b64);
  const bytes = new Uint8Array(new ArrayBuffer(bin.length));
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return bytes;
}

// —— P2P 节点网络（os-p2p 组网层观察面；设计 docs/NEXOS_P2P_NETWORK_DESIGN.md）——

/** GET /api/v1/p2p/status 响应：自身身份（NodeID=secp256k1 公钥 / OverlayAddr=EVM 同源）+ 监听。 */
export interface P2pStatus {
  enabled: boolean;
  self: {
    /** 0x + 66 hex 压缩公钥（= chain_auth 链上身份，节点即用户）。 */
    node_id: string;
    /** 0x + 40 hex（EVM 地址派生同源）。 */
    overlay_addr: string;
    /** 节点昵称（NEXOS_P2P_NAME，可空）。 */
    name: string;
    /** 公网服务节点（bootstrap 锚点 + 端点交换所 + relay 志愿者）。 */
    public: boolean;
  } | null;
  /** 监听地址（如 0.0.0.0:7070）。 */
  listen: string | null;
  /** 路由表已知节点总数。 */
  peers_known: number;
  /** 当前已认证直连数。 */
  peers_connected: number;
}

/** GET /api/v1/p2p/peers 数组元素：路由表条目 + 连接状态。 */
export interface P2pPeer {
  /** 0x + 66 hex 压缩公钥。 */
  id: string;
  /** 可拨 underlay（null = NAT 后节点，只能打洞/中继）。 */
  underlay: string | null;
  /** 公网服务节点标志。 */
  public: boolean;
  /** NODES 学到的中继者（0x…66 hex，可空）。 */
  relay: string | null;
  /** 当前是否有已认证直连。 */
  connected: boolean;
  /** 是否经我中继（我是它的 relay）。 */
  relayed_by_me: boolean;
  /** 我掌握的可达性路由 {该节点 → 经谁}（可空）。 */
  route_via: string | null;
}

/** GET /api/v1/p2p/buckets 响应：非空 k-bucket 摘要 + 观测端点簿（地址交换所）。 */
export interface P2pBucketsResp {
  /** 非空桶列表（po = 邻域阶 0..=159，越大越近；每桶 k=16 上限）。 */
  buckets: { po: number; count: number; entries: string[] }[];
  /** 观测端点簿 {NodeID → 网络观测 ip:port}（NAT 映射口，打洞目标）。 */
  known_endpoints: { id: string; addr: string }[];
}

/** GET /api/v1/p2p/ladder 响应：连接阶梯统计。 */
export interface P2pLadderStats {
  direct: number;
  punched: number;
  relayed: number;
  punch_failed: number;
}

/** GET /api/v1/p2p/identity-conflicts 数组元素：身份冲突观测条目。
 *  同一公钥（NodeID=本机）从其他地址连接的本地警告观测面——仅提示不阻断
 *  （身份=密钥：多个 OS 用同一私钥进入时权限共享是设计特性）。内存态，重启清除。 */
export interface P2pIdentityConflict {
  /** 冲突 NodeID（= 本机 NodeID，0x+66 hex）。 */
  node_id: string;
  /** 对端观测地址 ip:port（同公钥从不同地址进入的判据）。 */
  remote_addr: string;
  /** 首次发现（unix 秒）。 */
  first_seen: number;
  /** 最近发现（unix 秒）。 */
  last_seen: number;
  /** 累计警告次数。 */
  warning_count: number;
}

// —— 节点元数据注册表（os-p2p meta 组件——节点存活判定的唯一账本）——

/** meta 注册表条目的存活状态（外部标签式 JSON：{"active":…} / {"inactive":…}）。 */
export type P2pMetaState =
  /** 活跃（心跳周期内；consec_fail 连续失败计数，到 5 出局）。 */
  | { active: { score: number; consec_fail: number } }
  /** 非活跃（五振出局，不再心跳；since = 出局时刻 unix 秒）。 */
  | { inactive: { since: number } };

/** GET /api/v1/p2p/node-meta 数组元素：注册表条目
 *  （按健康分降序、Inactive 殿后；地址历史最新在前，上限 8 条）。 */
export interface P2pNodeMetaEntry {
  /** 0x + 66 hex 压缩公钥。 */
  id: string;
  /** 观测地址历史（去重，最新在前，上限 8 条）。 */
  addrs: string[];
  /** 首次见到（unix 秒）。 */
  first_seen: number;
  /** 最近一次确认存活（unix 秒）。 */
  last_seen: number;
  /** 存活状态（active 带健康分 / inactive 带出局时刻）。 */
  state: P2pMetaState;
  /** 知识来源：direct（本机直连观测）/ gossip（他节点转述）。 */
  source: 'direct' | 'gossip';
}

// —— WAN 出口共享 + 防火墙（handlers/network_exit.rs；docs/NETWORK_EXIT_RELAY.md）——

/** GET /api/v1/net-exit/status 响应：出口状态全景。 */
export interface NetExitStatus {
  /** 组网是否启用（恒 true——未启用时端点 503）。 */
  enabled: boolean;
  /** 本节点 NodeID（0x + 66 hex）。 */
  node_id: string;
  /** 本节点是否声明出口（offer——digest 自广播 exit_offered 位）。 */
  offered: boolean;
  /** 已授权且未过期的节点（exit_for——本节点为它们出网）。 */
  exit_for: string[];
  /** 授权表全量（含已过期条目，expired 标记）。 */
  authorizations: NetExitAuthorization[];
  /** 经 gossip 学到的出口节点（digest exit_offered=true 的注册表条目）。 */
  known_exits: NetExitKnownExit[];
  /** 默认出口节点（null = 未设置）。 */
  default_exit: string | null;
  /** 入口侧本地 SOCKS5 地址（浏览器/系统代理指向这里——v2ray 客户端模式）。 */
  local_socks: string;
  /** 出口侧本地 SOCKS5 地址（本机代拨入口）。 */
  exit_socks: string;
  /** 当前活跃中继连接数。 */
  active_conns: number;
  /** 观测计数。 */
  stats: { conns_opened: number; conns_refused: number; bytes_relayed: number };
}

/** net-exit 授权表条目。 */
export interface NetExitAuthorization {
  /** 被授权节点（NodeID hex）。 */
  node_id: string;
  /** 授权时刻（unix 秒）。 */
  granted_at: number;
  /** 过期时刻（unix 秒）。 */
  expires_at: number;
  /** 是否已过期。 */
  expired: boolean;
}

/** 已知出口节点（digest 学到）。 */
export interface NetExitKnownExit {
  /** NodeID hex。 */
  node_id: string;
  /** 最近确认存活（unix 秒）。 */
  last_seen: number;
  /** 是否活跃。 */
  alive: boolean;
}

/** GET /api/v1/firewall/rules 数组元素：防火墙规则（空表起步——真实数据）。 */
export interface FirewallRule {
  /** 规则 ID（fw-<n>）。 */
  id: string;
  /** 方向：in（INPUT 链）/ out（OUTPUT 链）。 */
  direction: 'in' | 'out';
  /** 协议：tcp / udp / icmp / any。 */
  proto: 'tcp' | 'udp' | 'icmp' | 'any';
  /** 目标端口（null = 不限）。 */
  port: number | null;
  /** 源 CIDR/IP（"any" = 不限）。 */
  source: string;
  /** 动作：allow（ACCEPT）/ deny（DROP）。 */
  action: 'allow' | 'deny';
  /** 是否启用（apply 只翻译 enabled 规则）。 */
  enabled: boolean;
  /** 备注。 */
  note: string;
}

/** POST /api/v1/firewall/apply 响应。 */
export interface FirewallApplyResp {
  /** 全部命令是否执行成功（sudo/iptables 不可用 → false + warning）。 */
  applied: boolean;
  /** 规则总数。 */
  rules_total: number;
  /** 启用规则数。 */
  rules_enabled: number;
  /** 管理链名。 */
  chains: string[];
  /** 逐条命令执行记录。 */
  commands: { cmd: string; ok: boolean; fail_ok?: boolean; stderr?: string; error?: string }[];
  /** 降级提示（applied=false 时非空）。 */
  warning: string;
}

/** GET /api/v1/firewall/status 响应：iptables 链实况回读。 */
export interface FirewallStatusResp {
  chains: Record<string, { chain: string; ok: boolean; raw?: string; lines?: string[]; error?: string }>;
  note: string;
}

// —— 节点发现页聚合视图（handlers/node_view.rs；os-p2p 真实数据接线）——

/** GET /api/v1/nodes/combined 的 lan[] 元素：局域网邻居（underlay 私网地址的直连 peer）。 */
export interface CombinedLanNode {
  /** 0x + 66 hex 压缩公钥。 */
  node_id: string;
  /** 直连可拨地址 ip:port。 */
  addr: string;
  /** 当前是否有已认证直连（绿点/灰点）。 */
  connected: boolean;
  /** 公网服务节点标志。 */
  public: boolean;
  /** 角色：hub（公网服务节点）/ edge（普通节点）。 */
  role: 'hub' | 'edge';
  /** 存活状态（meta 组件账本）：本分组内恒 active——inactive 的节点已移入
   *  inactive 分组（无 meta 条目的直连 peer 默认 active）。 */
  status: 'active' | 'inactive';
  /** 健康分 0-100（无元数据条目时 0）。 */
  score: number;
  /** 最近一次确认存活（unix 秒；无条目时 0）。 */
  last_seen: number;
  /** 元数据来源：direct（本机直连观测）/ gossip（他节点转述）/ null（无条目）。 */
  meta_source: 'direct' | 'gossip' | null;
  /** 对方 IM 大厅开放状态（P2P 探针缓存）：true=可进 IM 远程大厅 / false=未开放
   *  （按钮灰）/ null=查询在途或短 ID 不可查。 */
  im_public: boolean | null;
}

/** GET /api/v1/nodes/combined 的 p2p[] 元素：P2P/WAN 远端节点。 */
export interface CombinedP2pNode {
  /** 0x + 66 hex（bucket 来源为短式 0x1234…cdef）。 */
  node_id: string;
  /** 可拨 underlay（null = NAT 后节点，只能打洞/中继）。 */
  addr: string | null;
  connected: boolean;
  public: boolean;
  /** NODES 学到的中继者。 */
  relay: string | null;
  /** 可达性路由 {该节点 → 经谁}（"来源节点"展示）。 */
  route_via: string | null;
  /** 来源：peer（路由表直连集）/ bucket（Kademlia 桶非直连）。 */
  source: 'peer' | 'bucket';
  /** 存活状态（meta 组件账本；语义同 CombinedLanNode.status）。 */
  status: 'active' | 'inactive';
  /** 健康分 0-100（无元数据条目时 0）。 */
  score: number;
  /** 最近一次确认存活（unix 秒；无条目时 0）。 */
  last_seen: number;
  /** 元数据来源：direct / gossip / null（bucket 短 ID 无从匹配时恒 null）。 */
  meta_source: 'direct' | 'gossip' | null;
  /** 对方 IM 大厅开放状态（语义同 CombinedLanNode.im_public；短 ID 恒 null）。 */
  im_public: boolean | null;
}

/** GET /api/v1/nodes/combined 的 inactive[] 元素：非活跃节点
 *  （meta 组件判定 Inactive——五振出局不再心跳；复活靠手动心跳或他节点报告）。 */
export interface CombinedInactiveNode {
  /** 0x + 66 hex 全量 ID（来自元数据注册表，可直接寻址手动心跳）。 */
  node_id: string;
  /** 观测地址历史（去重，最新在前，上限 8 条）。 */
  addrs: string[];
  /** 健康分（Inactive 出局即不再携带——恒 0）。 */
  score: number;
  /** 最近一次确认存活（unix 秒）。 */
  last_seen: number;
  /** 元数据来源：direct（本机直连观测）/ gossip（他节点转述）。 */
  meta_source: 'direct' | 'gossip';
  /** 出局时刻（unix 秒——"非活跃多久"的起点）。 */
  since: number;
}

/** GET /api/v1/nodes/combined 的 self：本机信息（P2P 未启用时仅 hostname 兜底）。 */
export interface CombinedSelf {
  enabled: boolean;
  node_id: string | null;
  overlay_addr: string | null;
  /** 节点昵称（NEXOS_P2P_NAME，未设置为 null）。 */
  name: string | null;
  /** 本机主机名（始终可见）。 */
  hostname: string;
  public: boolean;
  /** hub / edge（未启用为 null）。 */
  role: 'hub' | 'edge' | null;
  listen: string | null;
}

/** GET /api/v1/nodes/combined 响应：节点发现页一次拉全（LAN / P2P / 非活跃三段 + 自机条）。 */
export interface CombinedNodes {
  lan: CombinedLanNode[];
  p2p: CombinedP2pNode[];
  /** 非活跃节点（meta 判定 Inactive——不混入 lan/p2p）。 */
  inactive: CombinedInactiveNode[];
  ladder: P2pLadderStats;
  self: CombinedSelf;
}

// —— 更新（update：NexHub tag 更新源 + 通道过滤 + A/B 槽位任务）——
/** 更新通道 id（tag 过滤策略）：stable 正式 / beta *-beta* / nightly 全收 / manual 仅手动。 */
export type UpdateChannel = 'stable' | 'beta' | 'nightly' | 'manual';
/** 通道元信息（GET /api/v1/update/channels 项）。 */
export interface UpdateChannelInfo {
  id: UpdateChannel;
  name: string;
  description: string;
}
/** GET /api/v1/update/channels 响应。 */
export interface UpdateChannelsResp {
  current: UpdateChannel;
  channels: UpdateChannelInfo[];
}
/** 单条可用更新（check 结果项 / status 待应用清单项）。 */
export interface UpdateAvailableItem {
  /** NexHub 仓库 tag 名（如 v0.2.0） */
  tag: string;
  /** 解析后的 semver 版本（0.2.0） */
  version: string;
  /** 归属通道桶：stable / beta / prerelease（其它预发布） */
  channel: string;
  /** 打 tag 时间（git creatordate；不可解析为 null） */
  created_at: string | null;
}
/** 槽位状态（os-update SlotState 序列化）。 */
export interface UpdateSlotState {
  slot: 'a' | 'b';
  status: 'active' | 'inactive' | 'failed' | 'updating';
  version: string | null;
  last_activated_at: string | null;
  last_written_at: string | null;
}
/** GET /api/v1/update/status 响应。 */
export interface UpdateStatusResp {
  current_version: string;
  channel: UpdateChannel;
  slot_a: UpdateSlotState;
  slot_b: UpdateSlotState;
  active_slot: 'a' | 'b';
  writable_slot: 'a' | 'b';
  last_check: string | null;
  pending_updates: UpdateAvailableItem[];
  /** 状态 JSON 持久化路径（null = 内存态） */
  state_path: string | null;
}
/** POST /api/v1/update/check 响应。 */
export interface UpdateCheckResp {
  current_version: string;
  channel: UpdateChannel;
  available: UpdateAvailableItem[];
  checked_at: string;
  /** 实际采用的更新源（本地裸仓库路径或远端 git URL；均不可达时为本地路径） */
  repo: string;
  /** 更新源模式三态：local 本地裸仓库 / remote 远端 git URL / none 均不可达 */
  repo_mode: 'local' | 'remote' | 'none';
  /** 配置的远端更新源 URL（env NEXOS_UPDATE_REPO_URL；未配置为 null） */
  repo_url: string | null;
  /** false = 更新源不可达（本地与远端均失败：仓库缺失/git 失败/URL 不可达/无 tag），降级为空清单 */
  repo_reachable: boolean;
}
/** 更新任务状态机阶段。 */
export type UpdateTaskStatus =
  | 'pending'
  | 'downloading'
  | 'verifying'
  | 'writing'
  | 'reboot_pending'
  | 'done'
  | 'failed';

/** 更新任务（POST /apply 响应 / GET tasks 项；GET tasks/:id 每次轮询推进一步）。 */
export interface UpdateTask {
  id: string;
  version: string;
  tag: string | null;
  channel: UpdateChannel;
  status: UpdateTaskStatus;
  /** 写入目标槽（A/B 双槽语义：始终是"另一槽"） */
  slot_target: 'a' | 'b';
  /** 0-100（阶段推进启发值） */
  progress: number;
  created_at: string;
  updated_at: string;
  error: string | null;
  /** 预留说明（writing→reboot_pending 起写入：真实镜像 I/O 待接入） */
  note: string | null;
}

// —— 开发者中心（devdocs：仓库 docs/ 目录只读索引 + Markdown 原文）——
/** 一篇文档的索引项（GET /api/v1/devdocs/index docs 数组元素）。 */
export interface DevDocEntry {
  /** 相对文档根路径（GET doc/*path 直用，如 `dev/01-app-development.md`） */
  path: string;
  /** 标题（正文首个 `# ` 行；无则回退文件名） */
  title: string;
  /** 分类：frontmatter `category:` > 一级子目录名 > `docs` */
  category: string;
  /** 字节数 */
  size: number;
  /** 最后修改时间（ISO；未知为 null） */
  mtime: string | null;
}
/** GET /api/v1/devdocs/index 响应。 */
export interface DevDocsIndexResp {
  docs: DevDocEntry[];
  /** 分类名列表（出现顺序，目录树分组用） */
  categories: string[];
  /** false = 本节点未检出仓库（降级模式：空清单 + note 提示） */
  source_available: boolean;
  /** 实际使用的文档根路径 */
  root: string;
  /** 降级提示（source_available=false 时非空） */
  note: string | null;
}
/** GET /api/v1/devdocs/doc/*path 响应。 */
export interface DevDocResp {
  path: string;
  title: string;
  /** Markdown 原文（前端 marked 渲染） */
  markdown: string;
  mtime: string | null;
}

// —— 「管理」桌面应用（Web 终端）契约（/api/v1/terminal/*，docs/ADMIN_CONSOLE.md）——

/** 活跃终端会话（GET/POST /api/v1/terminal/sessions，全部 admin）。 */
export interface TerminalSession {
  session_id: string;
  /** `"local"` | `"ssh"` */
  kind: string;
  /** 展示目标：「本地 shell」/「root@10.0.0.2:22」/「目标名（user@host:port）」 */
  target: string;
  cols: number;
  rows: number;
  created_at: string;
}

/** 创建终端会话请求体（POST /api/v1/terminal/sessions，admin）。 */
export interface TerminalCreateBody {
  kind: 'local' | 'ssh';
  /** 直连目标主机（ssh + 无 target_id 时必填） */
  host?: string;
  port?: number;
  user?: string;
  /** 直连私钥路径（限绝对路径；省略时用 ssh 缺省密钥） */
  key_path?: string;
  /** provisioning SSH 目标 id（提供时忽略直连参数，注册表只读复用） */
  target_id?: string;
  cols?: number;
  rows?: number;
}

/** GET /api/v1/terminal/node-snapshot 响应：节点常用状态快照（admin，
 *  管理页顶部状态条——点击各项往终端发对应快捷命令）。 */
export interface TerminalNodeSnapshot {
  /** 当前系统版本（与 /update/status 的 current_version 同源） */
  version: string;
  /** 主机在线时长（秒） */
  uptime_secs: number;
  /** P2P 已连接节点数；null = P2P 未启用（NEXOS_P2P_ENABLE 未开） */
  p2p_connected: number | null;
  /** 根分区使用率（0-100，一位小数） */
  disk_use_pct: number;
  /** 内存使用率（0-100，一位小数） */
  mem_use_pct: number;
}

// —— vLLM Recipes 导入（配方库；handlers/llm.rs 烘焙代理，公开读）——

/** GET /api/v1/llm/recipes/catalog 数组元素（上游 models.json 精简目录）。 */
export interface LlmRecipeCatalogItem {
  /** HF 模型 ID（查单配方 / 拼官网链接的键），如 meta-llama/Llama-3.1-8B。 */
  hf_id: string;
  title: string;
  provider: string;
  /** 上游索引当前不提供该字段 → null（配方详情 meta 才有）。 */
  date_updated?: string | null;
}

/** 单配方精度变体（variants 键值对的值；缺字段容忍）。 */
export interface LlmRecipeVariant {
  precision?: string;
  vram_minimum_gb?: number;
  model_id?: string;
  description?: string;
  [k: string]: unknown;
}

/**
 * GET /api/v1/llm/recipes/recipe 响应（上游 JSON 原样透传，宽松索引签名）。
 * 上游可能增删字段——消费方一律按可缺省渲染，勿假设必填。
 */
export interface LlmRecipeDetail {
  hf_id?: string;
  meta?: {
    title?: string;
    provider?: string;
    description?: string;
    date_added?: string;
    date_updated?: string;
    difficulty?: string;
    tasks?: string[];
    [k: string]: unknown;
  };
  recommended_command?: {
    hardware?: string;
    strategy?: string;
    docker_image?: string;
    /** 推荐启动命令（「复制启动命令」的复制源）。 */
    command?: string;
    docker_command?: string;
    env?: Record<string, string>;
    argv?: string[];
    [k: string]: unknown;
  };
  variants?: Record<string, LlmRecipeVariant>;
  /** 部署指南（markdown/text；上游内容，marked 渲染同 DevDocs 信任模型）。 */
  guide?: string;
  [k: string]: unknown;
}

/** 「存为本地配方」的 localStorage 记录（llm-recipes-saved；轻量本地留存）。 */
export interface LlmRecipeSaved {
  hf_id: string;
  title: string;
  provider: string;
  command: string;
  docker_command: string;
  hardware: string;
  variants?: Record<string, LlmRecipeVariant>;
  saved_at: string;
}

// —— vLLM 实例拉起日志（handlers/llm.rs，公开读）——

/** GET /api/v1/llm/instances/:id/log 响应（按实例日志尾，2026-08-31）。 */
export interface LlmInstanceLog {
  instance_id: string;
  /** 日志尾 N 行（默认 200、上限 1000；按文件顺序）。 */
  lines: string[];
  /** 日志文件绝对路径（单文件模式 NEXOS_LLM_SPAWN_LOG 下为该共享文件）。 */
  file: string;
  /** 实例当前 status（starting 时看启动进度最常用）。 */
  status: string;
}

// —— 推理环境（vLLM Python venv 管理；handlers/llm_envs.rs，公开读列表）——

/** GET /api/v1/llm/environments 的 environments 数组元素（真实注册表行）。 */
export interface LlmEnvRow {
  name: string;
  /** venv 绝对路径（<NEXOS_LLM_ENVS_ROOT>/<name>）。 */
  path: string;
  python_version?: string | null;
  /** 请求的 vLLM 版本（latest 或具体版本号；nightly 渠道恒 latest）。 */
  vllm_version_requested?: string | null;
  /** 探测到的已装版本（importlib.metadata 真实输出；未就绪 null）。 */
  vllm_version_installed?: string | null;
  /** 安装渠道：'stable'（默认）| 'nightly'（预置示例恒最新；存量行缺省 stable）。 */
  channel?: string | null;
  is_default: boolean;
  /** creating | updating | ready | error。 */
  status: string;
  size_bytes: number;
  /** Unix epoch 秒。 */
  created_at: number;
  updated_at: number;
  last_error?: string | null;
}

/** GET /api/v1/llm/environments/tasks/:id 响应（列表元素同字段无 log）。 */
export interface LlmEnvTask {
  id: string;
  /** create | update。 */
  kind: string;
  env_name: string;
  /** running | done | error。 */
  status: string;
  started_at: number;
  finished_at?: number | null;
  /** 日志尾（环形上限 200 行；列表接口不带此字段）。 */
  log?: string[];
}

// —— 外部 API 接入（OpenAI 兼容端点登记/连通测试/对话直通；
//    handlers/llm_external.rs，llm 组件子模块，2026-08-31）——

/** GET /api/v1/llm/external-apis 的 apis 数组元素（api_key 永不出明文，只有脱敏串）。 */
export interface LlmExternalApi {
  id: string;
  name: string;
  /** OpenAI 兼容根地址（含 /v1）。 */
  base_url: string;
  /** 脱敏 key（sk-a***3456；空 = 未配置）。 */
  api_key_masked: string;
  has_api_key: boolean;
  /** 可用模型 id 列表（可由连通测试回填）。 */
  models: string[];
  /** unknown | ok | error（最近一次真实连通测试结果）。 */
  status: string;
  last_check_at?: string | null;
  notes?: string | null;
  /** 来源 NodeID（0x+66hex；联邦大厅一键导入写入，2026-09-02。非空 →
   * chat/test 经 overlay 定向该源节点代发（跨网可达）；空 = 直连。 */
  via_node?: string;
  created_at: string;
}

/** POST /api/v1/llm/external-apis/:id/test 响应（真实 GET <base>/models 探测产物）。 */
export interface LlmExternalTestResult {
  id: string;
  ok: boolean;
  /** 上游 /models 返回的模型 id 清单（ok=false 时空数组）。 */
  models: string[];
  /** 真实计时延迟（毫秒）。 */
  latency_ms: number;
  error?: string | null;
}

/** 可导入（已导出/未导入）的 ZFS 池条目（GET /api/v1/disks/importable）。 */
export interface ImportablePool {
  /** 池名（如 nvme）。 */
  name: string;
  /** 池 GUID（id 行原文）。 */
  id: string;
  /** 池状态（ONLINE / DEGRADED…）。 */
  state: string;
  /** 该池分段原文（含 config 盘列表；排障用）。 */
  raw: string;
}

/**
 * ZFS 工具不可用时读端点的降级空态（storage.rs 2026-09-02）。
 *
 * 无 zfsutils 的节点（如 install.sh 最小节点）上，pools / datasets / snapshots
 * 读端点不再 500，而是 200 + `{<key>: [], zfs_available: false}`；可用路径仍返回
 * 裸数组（零形状变更）。importable 始终是对象，可用时额外带 `zfs_available: true`。
 */
export interface ZfsUnavailable {
  zfs_available: false;
  [listKey: string]: unknown;
}

/** 类型守卫：响应是否为「本节点未安装 ZFS 工具」的降级空态。 */
export function isZfsUnavailable(v: unknown): v is ZfsUnavailable {
  return (
    !!v &&
    typeof v === 'object' &&
    !Array.isArray(v) &&
    (v as Record<string, unknown>).zfs_available === false
  );
}

/** GET /api/v1/disks/importable 响应（探测失败后端降级为空数组）。 */
export interface ImportablePoolsResp {
  importable: ImportablePool[];
  /** ZFS 工具可用性（不可用时 importable 恒为空数组；降级时恒 false）。 */
  zfs_available?: boolean;
}

/** POST /api/v1/disks/import 响应（成功后 pool 为新导入池的完整信息）。 */
export interface ImportPoolResp {
  ok: boolean;
  action?: string;
  pool?: Pool | null;
}

/**
 * DELETE /api/v1/pools/:name 响应（TrueNAS Export/Destroy 式删池）。
 * - wipe=false（默认）：zpool export——磁盘 ZFS 标签保留，池出现在可导入探测里；
 * - wipe=true：zpool destroy + 逐盘 wipefs -a——盘变完全空白（数据不可恢复）。
 */
export interface PoolDeleteResp {
  ok: boolean;
  /** 'export'（保留标签）| 'destroy'（彻底擦除）。 */
  action: 'export' | 'destroy';
  /** 已删除的池名。 */
  destroyed: string;
  /** 是否执行了彻底擦除。 */
  wipe: boolean;
  /** 删池前从 zpool status 抓取的成员盘裸名列表。 */
  members: string[];
  /** wipe=true 时成功擦除的成员盘列表（wipe=false 恒空）。 */
  wiped_disks: string[];
  /** 单盘擦除失败明细（池已删，逐盘如实上报；成功时缺省）。 */
  wipe_errors?: { disk: string; error: string }[];
  /** 降级告警（如成员盘探测失败但 export 仍成功）。 */
  warning?: string;
}

export const endpoints = {
  // —— 系统 ——
  status: (): Promise<SystemStatus> => get<SystemStatus>('/status'),
  health: (): Promise<{ status: string }> => get<{ status: string }>('/healthz'),
  version: (): Promise<VersionInfo> => get<VersionInfo>('/api/v1/version'),
  virtCheck: (): Promise<unknown> => get('/api/v1/system/virt-check'),
  /**
   * 能力快照（GET /api/v1/capabilities，读公开，v0.1.28）。
   * 【同步注释】本端点的**权威消费者**是 @nexos/app-sdk（src/sdk/capabilities.ts
   * ——带 5s 缓存/订阅/降级三态派生）；应用包一律走 SDK（ctx.sdk 或
   * `import { createSdk } from '@nexos/app-sdk'`），不要直连本封装。
   * 响应形状与服务端 handlers/capabilities.rs 的 serde 逐字段对齐。
   */
  capabilities: (): Promise<import('../sdk/capabilities').CapabilitySnapshot> =>
    get('/api/v1/capabilities'),

  // —— 存储 ——
  /** 列存储池；ZFS 工具不可用时返回 ZfsUnavailable 降级空态（200，非错误）。 */
  pools: (pool?: string): Promise<Pool | Pool[] | ZfsUnavailable> =>
    pool
      ? get<Pool>(`/api/v1/pools/${encodeURIComponent(pool)}`)
      : get<Pool[]>('/api/v1/pools'),
  /** 列数据集；ZFS 工具不可用时返回 ZfsUnavailable 降级空态。 */
  datasets: (pool?: string): Promise<Dataset[] | ZfsUnavailable> =>
    get<Dataset[]>(
      pool ? `/api/v1/datasets?pool=${encodeURIComponent(pool)}` : '/api/v1/datasets',
    ),
  /** 列快照；ZFS 工具不可用时返回 ZfsUnavailable 降级空态。 */
  snapshots: (ds?: string): Promise<Snapshot[] | ZfsUnavailable> =>
    get<Snapshot[]>(
      ds ? `/api/v1/snapshots?dataset=${encodeURIComponent(ds)}` : '/api/v1/snapshots',
    ),
  createPool: (req: CreatePoolRequest): Promise<Pool> =>
    post<Pool>('/api/v1/pools', req),
  /** 列出本机可用磁盘（GET /api/v1/disks，lsblk 探测，已过滤系统盘/loop）。
   *  has_partitions=true 的盘残留分区表/签名，需先初始化才能建池。 */
  disks: (): Promise<DiskInfo[]> => get<DiskInfo[]>('/api/v1/disks'),
  /**
   * 磁盘分区详情（GET /api/v1/disks/:name/partitions）。
   * name 为**裸设备名**（如 nvme1n1，不带 /dev/ 前缀）。只读公开；
   * 设备不存在时后端降级 200 + warning。
   */
  diskPartitions: (name: string): Promise<DiskPartitions> =>
    get<DiskPartitions>(`/api/v1/disks/${encodeURIComponent(name)}/partitions`),
  /**
   * 初始化磁盘（POST /api/v1/disks/:name/initialize，需 admin）。
   * wipefs -a 清除整盘全部分区表与签名（BitLocker/GPT/MBR 等），不可恢复——
   * 调用方必须先完成两步确认。name 为裸设备名。
   */
  initializeDisk: (name: string): Promise<InitializeDiskResp> =>
    post<InitializeDiskResp>(`/api/v1/disks/${encodeURIComponent(name)}/initialize`, {}),
  /**
   * 探测可导入（已导出/未导入）的 ZFS 池（GET /api/v1/disks/importable）。
   * 只读 `zpool import` 列表（**不真导入**）；失败降级为空数组。
   */
  importablePools: (): Promise<ImportablePoolsResp> =>
    get<ImportablePoolsResp>('/api/v1/disks/importable'),
  /**
   * 导入一个已导出的 ZFS 池（POST /api/v1/disks/import，需 admin）。
   * 仅在用户显式确认后调用；失败（池名冲突 409 / 权限 401 / 其他 400）
   * 抛 ApiError，message 携带 zpool 原始 stderr。
   */
  importPool: (name: string): Promise<ImportPoolResp> =>
    post<ImportPoolResp>('/api/v1/disks/import', { name }),
  /**
   * 删除存储池（DELETE /api/v1/pools/:name?wipe=，需 admin）。
   * - wipe=false（默认，「仅删除池」）：zpool export，磁盘标签保留 → 池出现在
   *   「可导入的存储池」横幅，可重新导入（数据不丢）；
   * - wipe=true（「彻底擦除」）：zpool destroy + 逐盘 wipefs -a，盘变完全空白
   *   → 出现在创建池向导可选列表（**数据不可恢复**）。
   * 调用方必须先完成「输入池名确认」对话框（TrueNAS 式防误删）。失败（busy 409 /
   * 权限 401 / 池不存在 404 / 其他 400）抛 ApiError，message 携带 zpool 原始 stderr。
   */
  poolDelete: (name: string, wipe = false): Promise<PoolDeleteResp> =>
    del<PoolDeleteResp>(
      `/api/v1/pools/${encodeURIComponent(name)}?wipe=${wipe ? '1' : '0'}`,
    ),

  // —— 计算（VM）——
  vms: (): Promise<Vm[]> => get<Vm[]>('/api/v1/vms'),
  vm: (id: string): Promise<Vm> => get<Vm>(`/api/v1/vms/${encodeURIComponent(id)}`),
  vmStart: (id: string): Promise<unknown> =>
    post(`/api/v1/vms/${encodeURIComponent(id)}/start`, {}),
  vmStop: (id: string): Promise<unknown> =>
    post(`/api/v1/vms/${encodeURIComponent(id)}/stop`, {}),
  vmDelete: (id: string): Promise<unknown> =>
    del(`/api/v1/vms/${encodeURIComponent(id)}`),
  vmCreate: (req: CreateVmRequest): Promise<Vm> => post<Vm>('/api/v1/vms', req),

  // —— 共享 ——
  shares: (): Promise<Share[]> => get<Share[]>('/shares'),
  exports: (): Promise<NfsExport[]> => get<NfsExport[]>('/api/v1/exports'),

  // —— 用户 ——
  users: (includeDisabled = false): Promise<User[]> =>
    get<User[]>(`/api/v1/users${includeDisabled ? '?include_disabled=1' : ''}`),

  // —— 节点 ——
  nodes: (): Promise<NodeInfo[]> => get<NodeInfo[]>('/discover/nodes'),
  node: (id: string): Promise<NodeInfo> =>
    get<NodeInfo>(`/api/v1/nodes/${encodeURIComponent(id)}`),
  /** 节点发现页聚合视图（GET /api/v1/nodes/combined，os-p2p 真实数据：lan/p2p/ladder/self）。 */
  nodeCombined: (): Promise<CombinedNodes> => get<CombinedNodes>('/api/v1/nodes/combined'),

  // —— IM（聊天）——
  // 认证契约（批次 2）：全部 /api/v1/im/* 用户面端点要求 IM token
  // （Bearer <IM token>，经 ImOpts.imToken 覆盖全局 admin token 注入）；
  // 请求体的 sender/user 字段服务端一律忽略（从 token 反查 pubkey 填充）。
  /** 获取挑战 nonce（POST /api/v1/im/auth/challenge，公开）。 */
  imAuthChallenge: (pubkey: string): Promise<ImAuthChallengeResp> =>
    post<ImAuthChallengeResp>('/api/v1/im/auth/challenge', { pubkey }),
  /** 验签换取 IM token（POST /api/v1/im/auth/verify，公开；签名 65 字节 r||s||v hex）。 */
  imAuthVerify: (
    pubkey: string,
    nonce: string,
    signature: string,
  ): Promise<ImAuthVerifyResp> =>
    post<ImAuthVerifyResp>('/api/v1/im/auth/verify', { pubkey, nonce, signature }),
  /** 列出所有群组/对话（GET /api/v1/im/groups，需 IM token）。 */
  imGroups: (opts?: ImOpts): Promise<ImGroup[]> =>
    request<ImGroup[]>('/api/v1/im/groups', {
      method: 'GET',
      headers: imAuthHeaders(opts),
    }),
  /** 创建群组/对话（POST /api/v1/im/groups，需 IM token；owner=token 反查 pubkey）。 */
  imCreateGroup: (name: string, members?: string[], opts?: ImOpts): Promise<ImGroup> =>
    request<ImGroup>('/api/v1/im/groups', {
      method: 'POST',
      body: { name, members: members ?? [] },
      headers: imAuthHeaders(opts),
    }),
  /** 列出某群组/对话的消息（GET /api/v1/im/conversations/:id/messages，需 IM token）。 */
  imMessages: (id: string, opts?: ImOpts): Promise<ImMessage[]> =>
    request<ImMessage[]>(`/api/v1/im/conversations/${encodeURIComponent(id)}/messages`, {
      method: 'GET',
      headers: imAuthHeaders(opts),
    }),
  /**
   * 离线补拉：增量取会话消息（GET /api/v1/im/messages?conversation_id=&after_id=&limit=，
   * 需 IM token）。返回按插入序升序、严格晚于 afterId 的消息——WS 断线重连后把
   * 缺口按 id 去重追加即可；afterId 传 null 则从头升序取（可当通用升序分页）。
   * limit 服务端钳制 1..=200（默认 50）；群组/大厅非成员 403，未知会话 404。
   */
  imMessagesAfter: (id: string, afterId: string | null, opts?: ImOpts): Promise<ImMessage[]> =>
    request<ImMessage[]>(
      `/api/v1/im/messages?conversation_id=${encodeURIComponent(id)}` +
        (afterId ? `&after_id=${encodeURIComponent(afterId)}` : ''),
      { method: 'GET', headers: imAuthHeaders(opts) },
    ),
  /**
   * 向群组/对话发送消息（POST /api/v1/im/conversations/:id/messages，需 IM token）。
   * body：{content} + 可选扩展（extras）——sender_kind 展示层自声明
   * （"human"|"agent"，服务端白名单归一）；attachment:{file_id,...} 附件核对
   * （服务端按 file_id 查落盘记录，伪造 filename/size_bytes 一律被真值覆盖，
   * 未知 file_id 400）。mentions 恒由服务端从 content 解析（客户端传无效）。
   */
  imSendMessage: (
    id: string,
    content: string,
    opts?: ImOpts,
    extras?: ImSendExtras,
  ): Promise<ImMessage> =>
    request<ImMessage>(
      `/api/v1/im/conversations/${encodeURIComponent(id)}/messages`,
      { method: 'POST', body: { content, ...extras }, headers: imAuthHeaders(opts) },
    ),
  /** 列出已连接节点/对端（GET /api/v1/im/peers，公开）。 */
  imPeers: (): Promise<ImPeer[]> => get<ImPeer[]>('/api/v1/im/peers'),
  /** 添加节点/对端（POST /api/v1/im/peers，addr 形如 host:port，系统级 admin token）。 */
  imAddPeer: (addr: string): Promise<ImPeer> =>
    post<ImPeer>('/api/v1/im/peers', { addr }),
  /** 标记消息已读（POST /api/v1/im/messages/:id/read，需 IM token；user 服务端反查）。 */
  imMarkRead: (id: string, opts?: ImOpts): Promise<ImMessage> =>
    request<ImMessage>(`/api/v1/im/messages/${encodeURIComponent(id)}/read`, {
      method: 'POST',
      body: {},
      headers: imAuthHeaders(opts),
    }),
  /** 某对话未读消息数（GET /api/v1/im/conversations/:id/unread，需 IM token；user 服务端反查）。 */
  imUnread: (
    id: string,
    opts?: ImOpts,
  ): Promise<{ conversation_id: string; user: string; unread: number }> =>
    request(
      `/api/v1/im/conversations/${encodeURIComponent(id)}/unread`,
      { method: 'GET', headers: imAuthHeaders(opts) },
    ),
  /**
   * 搜索消息（GET /api/v1/im/search?q=&conversation_id=&limit=，需 IM token）。
   * - conversationId 缺省 = 搜大厅（lobby）；指定 = 搜该会话（群组/大厅须成员，
   *   服务端 403/404 同离线补拉）
   * - limit 默认 50、服务端钳制 1..=200；结果 created_at 倒序（最新在前）
   * - 返回 q 原文回显（前端高亮用）+ 匹配 Message 列表
   */
  imSearch: (
    q: string,
    opts?: ImOpts & { conversationId?: string; limit?: number },
  ): Promise<{
    q: string;
    query: string;
    conversation_id: string;
    count: number;
    results: ImMessage[];
  }> => {
    const p = new URLSearchParams({ q });
    if (opts?.conversationId) p.set('conversation_id', opts.conversationId);
    if (opts?.limit) p.set('limit', String(opts.limit));
    return request(`/api/v1/im/search?${p.toString()}`, {
      method: 'GET',
      headers: imAuthHeaders(opts),
    });
  },

  // —— IM 大厅（公共频道：连接每一个超级个体；全部需 IM token）——
  /** 大厅信息（GET /api/v1/im/lobby；Bearer 即心跳，新用户自动加入 + 欢迎广播）。 */
  imLobby: (opts?: ImOpts): Promise<ImLobbyInfo> =>
    request<ImLobbyInfo>('/api/v1/im/lobby', {
      method: 'GET',
      headers: imAuthHeaders(opts),
    }),
  /** 大厅最近 50 条消息（GET /api/v1/im/lobby/messages；Bearer 即心跳）。 */
  imLobbyMessages: (opts?: ImOpts): Promise<ImMessage[]> =>
    request<ImMessage[]>('/api/v1/im/lobby/messages', {
      method: 'GET',
      headers: imAuthHeaders(opts),
    }),
  /**
   * 大厅离线补拉（GET /api/v1/im/lobby/messages?after_id=，需 IM token；Bearer 即心跳）。
   * 语义同 imMessagesAfter：返回严格晚于 afterId 的大厅消息（插入序升序），
   * 上限沿用该端点的最近 50 条。
   */
  imLobbyMessagesAfter: (afterId: string, opts?: ImOpts): Promise<ImMessage[]> =>
    request<ImMessage[]>(
      `/api/v1/im/lobby/messages?after_id=${encodeURIComponent(afterId)}`,
      { method: 'GET', headers: imAuthHeaders(opts) },
    ),
  /**
   * 发大厅消息（POST /api/v1/im/lobby/messages，需 IM token；sender 服务端反查）。
   * body 同构 imSendMessage：{content} + 可选 extras（sender_kind/attachment；
   * 空白 content 服务端 400）。mentions 恒服务端解析；@NexOS助手 触发内置助手。
   */
  imLobbySend: (content: string, opts?: ImOpts, extras?: ImSendExtras): Promise<ImMessage> =>
    request<ImMessage>('/api/v1/im/lobby/messages', {
      method: 'POST',
      body: { content, ...extras },
      headers: imAuthHeaders(opts),
    }),
  /** 大厅成员列表（GET /api/v1/im/lobby/members，需 IM token，区分在线/离线）。 */
  imLobbyMembers: (opts?: ImOpts): Promise<{
    lobby_id: string;
    member_count: number;
    online_count: number;
    members: ImLobbyMember[];
  }> =>
    request('/api/v1/im/lobby/members', {
      method: 'GET',
      headers: imAuthHeaders(opts),
    }),

  // —— IM 联邦大厅（fed-lobby：跨节点共享频道，与我的大厅完全隔离的可写会话；
  //     全部需 IM token；2026-08-23 用户纠正批次）——
  /**
   * 联邦大厅信息（GET /api/v1/im/fed-lobby；Bearer 即心跳 + 自动加入）。
   * 响应形状同 imLobby（id 恒为 'fed-lobby'）；发言广播到所有连接的 NexOS
   * 节点，其他节点用户的发言也出现在该会话（fed: 前缀 + 🌐 来源标注）。
   */
  imFedLobby: (opts?: ImOpts): Promise<ImLobbyInfo> =>
    request<ImLobbyInfo>('/api/v1/im/fed-lobby', {
      method: 'GET',
      headers: imAuthHeaders(opts),
    }),
  /** 联邦大厅最近 50 条消息（GET /api/v1/im/fed-lobby/messages；Bearer 即心跳）。 */
  imFedLobbyMessages: (opts?: ImOpts): Promise<ImMessage[]> =>
    request<ImMessage[]>('/api/v1/im/fed-lobby/messages', {
      method: 'GET',
      headers: imAuthHeaders(opts),
    }),
  /**
   * 联邦大厅离线补拉（GET /api/v1/im/fed-lobby/messages?after_id=，需 IM token）。
   * 语义同 imLobbyMessagesAfter：严格晚于 afterId 的 fed-lobby 会话消息升序。
   */
  imFedLobbyMessagesAfter: (afterId: string, opts?: ImOpts): Promise<ImMessage[]> =>
    request<ImMessage[]>(
      `/api/v1/im/fed-lobby/messages?after_id=${encodeURIComponent(afterId)}`,
      { method: 'GET', headers: imAuthHeaders(opts) },
    ),
  /**
   * 联邦大厅发言（POST /api/v1/im/fed-lobby/messages，需 IM token；sender 服务端
   * 反查 pubkey）。body：{content, sender_kind?}（联邦通道不承载附件）；本地落库
   * + P2P 广播全部已连接节点 + WS 广播本节点在线用户。
   */
  imFedLobbySend: (
    content: string,
    opts?: ImOpts,
    extras?: Pick<ImSendExtras, 'sender_kind'>,
  ): Promise<ImMessage> =>
    request<ImMessage>('/api/v1/im/fed-lobby/messages', {
      method: 'POST',
      body: { content, ...extras },
      headers: imAuthHeaders(opts),
    }),

  // —— IM 附件（文档传输，base64-JSON 通道；docs/IM_AGENTS_AND_FILES.md §3.2）——
  /**
   * 上传 IM 附件（POST /api/v1/im/files，需 IM token；链上 token 走网关 JSON
   * 通道，multipart 不可行 → {filename, content_base64}，≤64MiB 前端自行预检）。
   * 201 → {file_id, url, filename, size_bytes, mime}；url 为含上传者自身
   * IM token 的相对直链（?token= 场景：浏览器/<img> 无法带 Bearer 头；24h）。
   * 400 缺字段/坏 base64；413 超 64MiB。上传后把 file_id 塞进发消息 extras.attachment。
   */
  imUploadFile: async (
    filename: string,
    contentBase64: string,
    opts?: ImOpts,
  ): Promise<ImFileUploadResp> => {
    // 64MiB → ~85MB base64 JSON：统一 15s 超时不敷使用，自带 5 分钟 AbortController
    // （经 request 的 signal 参数覆盖默认定时器）。
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), IM_FILE_UPLOAD_TIMEOUT_MS);
    try {
      return await request<ImFileUploadResp>('/api/v1/im/files', {
        method: 'POST',
        body: { filename, content_base64: contentBase64 },
        headers: imAuthHeaders(opts),
        signal: controller.signal,
      });
    } finally {
      clearTimeout(timer);
    }
  },
  /**
   * 构造附件直链下载 URL（GET /api/v1/im/files/:file_id?token=<IM token>）。
   * token 三选一鉴权里的 query 直链形态：Bearer 头 / ?token= IM token /
   * ?token= 系统 admin token——用于 window.open/`<a href>` 等无法带头的场景
   * （token 24h 有效；url 泄露面 = 持有者转发给谁，谨慎转发场景只传 file_id）。
   */
  imFileUrl: (fileId: string, imToken: string): string =>
    `/api/v1/im/files/${encodeURIComponent(fileId)}?token=${encodeURIComponent(imToken)}`,

  // —— IM 消息推送通知 webhook（docs/IM_AGENTS_AND_FILES.md §7）——
  /**
   * 注册消息推送 webhook（POST /api/v1/im/notify/register，需 IM token；
   * owner=token 反查 pubkey）。注册后 IM 一有匹配的新消息，服务端即向 url
   * 异步 POST 完整 Message JSON（Header `X-NexOS-Event: lobby_message |
   * conversation_message`，超时 5s，不含任何 token）——外部 agent 据此消除轮询。
   * events 缺省双开；conversationId 可选绑定单个会话（须存在且可读，
   * 群组须先 join，否则 404/403）。400 url/events 非法。
   */
  imNotifyRegister: (
    url: string,
    events?: ImNotifyEvent[],
    opts?: ImOpts,
  ): Promise<ImWebhookRecord> =>
    request<ImWebhookRecord>('/api/v1/im/notify/register', {
      method: 'POST',
      body: { url, events },
      headers: imAuthHeaders(opts),
    }),
  /**
   * 列出自己的推送 webhook（GET /api/v1/im/notify/list，需 IM token；
   * owner 身份过滤——只返回当前 token 注册的，含已自动注销的
   * （status="disabled"，last_error 有注销原因））。
   */
  imNotifyList: (opts?: ImOpts): Promise<ImWebhookRecord[]> =>
    request<ImWebhookRecord[]>('/api/v1/im/notify/list', {
      method: 'GET',
      headers: imAuthHeaders(opts),
    }),
  /**
   * 注销推送 webhook（DELETE /api/v1/im/notify/:id，需 IM token；仅 owner
   * 可注销——他人 id 403，未知 id 404，成功 {ok:true, deleted:true}）。
   */
  imNotifyUnregister: (
    id: string,
    opts?: ImOpts,
  ): Promise<{ ok: boolean; id: string; deleted: boolean }> =>
    request(`/api/v1/im/notify/${encodeURIComponent(id)}`, {
      method: 'DELETE',
      headers: imAuthHeaders(opts),
    }),

  // —— IM 联邦接收开关（2026-08-23：关闭后不收其他节点的大厅消息）——
  /**
   * 查询联邦接收开关（GET /api/v1/im/federation，需 IM token）。
   * enabled=false 时服务端 ingest 入口短路——远程大厅消息不落地、不推送；
   * 本地消息与联邦发送不受影响。
   */
  imFederationGet: (opts?: ImOpts): Promise<ImFederationStatus> =>
    request<ImFederationStatus>('/api/v1/im/federation', {
      method: 'GET',
      headers: imAuthHeaders(opts),
    }),
  /**
   * 切换联邦接收开关（POST /api/v1/im/federation，IM token 或系统 admin
   * token）。body {enabled}；返回 {enabled, note}（note 说明当前状态语义，
   * 关闭=暂停接收，本地消息与发送不受影响）。
   */
  imFederationSet: (enabled: boolean, opts?: ImOpts): Promise<ImFederationStatus> =>
    request<ImFederationStatus>('/api/v1/im/federation', {
      method: 'POST',
      body: { enabled },
      headers: imAuthHeaders(opts),
    }),

  // —— IM 大厅开放开关 + 远程大厅互联（2026-08-23，节点发现页「进入 IM」联动）——
  /**
   * 查询大厅开放开关（GET /api/v1/im/lobby/access，admin 或 IM token）。
   * lobby_public=false（默认）= 其他节点无法浏览本机大厅。
   */
  imLobbyAccessGet: (opts?: ImOpts): Promise<ImLobbyAccessStatus> =>
    request<ImLobbyAccessStatus>('/api/v1/im/lobby/access', {
      method: 'GET',
      headers: imAuthHeaders(opts),
    }),
  /**
   * 切换大厅开放开关（POST /api/v1/im/lobby/access，admin 或 IM token）。
   * body {lobby_public}；返回 {lobby_public, note}（note 说明当前状态语义）。
   */
  imLobbyAccessSet: (
    lobbyPublic: boolean,
    opts?: ImOpts,
  ): Promise<ImLobbyAccessStatus> =>
    request<ImLobbyAccessStatus>('/api/v1/im/lobby/access', {
      method: 'POST',
      body: { lobby_public: lobbyPublic },
      headers: imAuthHeaders(opts),
    }),
  /**
   * 拉取对方节点的大厅镜像（GET /api/v1/im/lobby/remote/:node_id，IM token）。
   * 经加密 P2P 通道查询：public=true 带最近 20 条脱敏消息（无附件内容）；
   * false=对方未开放（error="denied"）；null=对方无应答（error="timeout"）。
   * timeoutMs 300..=8000（默认 4000）。
   */
  imLobbyRemoteGet: (
    nodeId: string,
    timeoutMs?: number,
    opts?: ImOpts,
  ): Promise<ImRemoteLobbyView> => {
    const q = timeoutMs ? `?timeout_ms=${timeoutMs}` : '';
    return request<ImRemoteLobbyView>(
      `/api/v1/im/lobby/remote/${encodeURIComponent(nodeId)}${q}`,
      { method: 'GET', headers: imAuthHeaders(opts) },
    );
  },
  /**
   * 向对方节点的大厅远程发言（POST /api/v1/im/lobby/remote/:node_id/messages，
   * IM token）。服务端先查对方开放状态：denied 403 / 无应答 504 / 开放则经
   * P2P 联邦送达（落地以对方开关为准；刷新镜像可见）。
   */
  imLobbyRemoteSend: (
    nodeId: string,
    content: string,
    opts?: ImOpts,
  ): Promise<{ ok: boolean; node_id: string; note: string }> =>
    request(`/api/v1/im/lobby/remote/${encodeURIComponent(nodeId)}/messages`, {
      method: 'POST',
      body: { content },
      headers: imAuthHeaders(opts),
    }),

  // —— IM 点对点直通消息 DM（2026-08-30：大厅保留现状之外的独立私信通道）——
  /**
   * 列出对话（GET /api/v1/im/conversations，需 IM token）。dm-* 会话按成员过滤
   * （只返回自己是成员的——对方发起的私信也可见），members=双方 pubkey；
   * 普通对话沿用全员可见。前端据 dm- 前缀识别直通会话（发送走 imDmSend）。
   */
  imConversations: (opts?: ImOpts): Promise<ImConversationRecord[]> =>
    request<ImConversationRecord[]>('/api/v1/im/conversations', {
      method: 'GET',
      headers: imAuthHeaders(opts),
    }),
  /**
   * 查询直通消息开放开关（GET /api/v1/im/dm/access，admin 或 IM token）。
   * dm_open=false 时其他身份发给你的直通消息被拒（服务端 403 / 跨节点丢弃），
   * 你自己发出的私信不受影响。开发阶段默认允许。
   */
  imDmAccessGet: (opts?: ImOpts): Promise<ImDmAccessStatus> =>
    request<ImDmAccessStatus>('/api/v1/im/dm/access', {
      method: 'GET',
      headers: imAuthHeaders(opts),
    }),
  /**
   * 切换直通消息开放开关（POST /api/v1/im/dm/access，admin 或 IM token）。
   * body {dm_open}；返回 {dm_open, note}。
   */
  imDmAccessSet: (dmOpen: boolean, opts?: ImOpts): Promise<ImDmAccessStatus> =>
    request<ImDmAccessStatus>('/api/v1/im/dm/access', {
      method: 'POST',
      body: { dm_open: dmOpen },
      headers: imAuthHeaders(opts),
    }),
  /**
   * 发起点对点直通消息（POST /api/v1/im/dm，需 IM token；sender=token 反查）。
   * body：{to_pubkey, content, sender_kind?, to_node?}。服务端路由：对方身份在
   * 本节点 → 本地投递（dm_open 关则 403「对方未开放直通消息」）；否则经 P2P
   * 定向发送到对方节点（to_node 显式指定，或按服务端登记的回程路由自动定向；
   * 无路由 404）。响应 {message, conversation_id, route, note?}——会话 id 确定性
   * （dm- 前缀，双端一致）。跨节点通道不承载附件（文件不出双端）。
   */
  imDmSend: (
    toPubkey: string,
    content: string,
    opts?: ImOpts,
    extras?: { sender_kind?: ImSenderKind; to_node?: string },
  ): Promise<ImDmSendResp> =>
    request<ImDmSendResp>('/api/v1/im/dm', {
      method: 'POST',
      body: { to_pubkey: toPubkey, content, ...extras },
      headers: imAuthHeaders(opts),
    }),

  // —— 网络 ——
  /** 网卡列表（GET /api/v1/network/interfaces）。 */
  networkInterfaces: (): Promise<unknown> => get('/api/v1/network/interfaces'),
  /** 路由/网关列表（GET /api/v1/network/routes）。 */
  networkRoutes: (): Promise<unknown> => get('/api/v1/network/routes'),
  /** 网络状态概要（GET /api/v1/network/status）。 */
  networkStatus: (): Promise<unknown> => get('/api/v1/network/status'),
  /** 设置某网卡角色标签（POST /api/v1/network/interfaces/:name/role）。
   *  role 取值：normal / management / storage / pxe / dhcp / dns。 */
  setNicRole: (name: string, role: string): Promise<unknown> =>
    post(`/api/v1/network/interfaces/${encodeURIComponent(name)}/role`, { role }),

  // —— WAN 出口共享 + 防火墙（component network-exit；docs/NETWORK_EXIT_RELAY.md）——
  // net-exit 读写分离（读公开 / 写 admin），未启用（NEXOS_P2P_ENABLE 未设）时
  // net-exit 端点 503；防火墙端点不依赖组网照常可用。
  /** 出口状态全景（GET /api/v1/net-exit/status，公开）。 */
  netExitStatus: (): Promise<NetExitStatus> => get<NetExitStatus>('/api/v1/net-exit/status'),
  /** 切换本节点出口声明（POST /api/v1/net-exit/offer，admin）。 */
  netExitOffer: (enabled: boolean): Promise<{ offered: boolean; applied: boolean; note: string }> =>
    post<{ offered: boolean; applied: boolean; note: string }>('/api/v1/net-exit/offer', {
      enabled,
    }),
  /** 授权节点经本节点出网（POST /api/v1/net-exit/authorize，admin；默认 deny）。 */
  netExitAuthorize: (
    nodeId: string,
    ttlMin: number,
  ): Promise<NetExitAuthorization> =>
    post<NetExitAuthorization>('/api/v1/net-exit/authorize', {
      node_id: nodeId,
      ttl_min: ttlMin,
    }),
  /** 撤销授权（DELETE /api/v1/net-exit/authorize/:node_id，admin）。 */
  netExitRevoke: (nodeId: string): Promise<{ ok: boolean; node_id: string }> =>
    request<{ ok: boolean; node_id: string }>(
      `/api/v1/net-exit/authorize/${encodeURIComponent(nodeId)}`,
      { method: 'DELETE' },
    ),
  /** 设默认出口（POST /api/v1/net-exit/use，admin；null 清除）。 */
  netExitUse: (
    exitNodeId: string | null,
  ): Promise<{ default_exit: string | null; note: string }> =>
    post<{ default_exit: string | null; note: string }>('/api/v1/net-exit/use', {
      exit_node_id: exitNodeId,
    }),
  /** 经默认/指定出口探活（POST /api/v1/net-exit/proxy，admin）。 */
  netExitProbe: (
    host: string,
    port: number,
    exitNodeId?: string,
  ): Promise<{ ok: boolean; exit_node: string; error: string | null }> => {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 20_000);
    return request<{ ok: boolean; exit_node: string; error: string | null }>(
      '/api/v1/net-exit/proxy',
      { method: 'POST', body: { host, port, exit_node_id: exitNodeId }, signal: controller.signal },
    ).finally(() => clearTimeout(timer));
  },
  /** 防火墙规则列表（GET /api/v1/firewall/rules，公开；空表起步无 seed）。 */
  firewallRules: (): Promise<FirewallRule[]> => get<FirewallRule[]>('/api/v1/firewall/rules'),
  /** 添加防火墙规则（POST /api/v1/firewall/rules，admin；deny-22-in-any 需 force）。 */
  firewallRuleAdd: (
    rule: Partial<FirewallRule> & { force?: boolean },
  ): Promise<FirewallRule> =>
    post<FirewallRule>('/api/v1/firewall/rules', rule as Record<string, unknown>),
  /** 启/停规则（POST /api/v1/firewall/rules/:id/toggle，admin）。 */
  firewallRuleToggle: (id: string, enabled: boolean): Promise<FirewallRule> =>
    post<FirewallRule>(`/api/v1/firewall/rules/${encodeURIComponent(id)}/toggle`, { enabled }),
  /** 删除规则（DELETE /api/v1/firewall/rules/:id，admin）。 */
  firewallRuleDelete: (id: string): Promise<{ ok: boolean; id: string }> =>
    request<{ ok: boolean; id: string }>(
      `/api/v1/firewall/rules/${encodeURIComponent(id)}`,
      { method: 'DELETE' },
    ),
  /** 规则 → iptables 链落地（POST /api/v1/firewall/apply，admin；危险集需 force）。 */
  firewallApply: (force = false): Promise<FirewallApplyResp> =>
    post<FirewallApplyResp>('/api/v1/firewall/apply', { force }),
  /** iptables 链实况（GET /api/v1/firewall/status，公开）。 */
  firewallStatus: (): Promise<FirewallStatusResp> => get<FirewallStatusResp>('/api/v1/firewall/status'),

  // —— P2P 节点网络（os-p2p 组网层，component p2p；docs/NEXOS_P2P_NETWORK_DESIGN.md）——
  // 读公开（status/peers/buckets/ladder——网络页 5s 轮询数据源）；
  // 写 admin（send/connect）。未启用（NEXOS_P2P_ENABLE 未设）全部 503
  // {"error":"P2P 未启用（NEXOS_P2P_ENABLE=1）"}——调用方凭 503 展示开启指引。
  /** 自身身份/监听/启用态（GET /api/v1/p2p/status）。 */
  p2pStatus: (): Promise<P2pStatus> => get<P2pStatus>('/api/v1/p2p/status'),
  /** 路由表摘要（GET /api/v1/p2p/peers）→ P2pPeer[]。 */
  p2pPeers: (): Promise<P2pPeer[]> => get<P2pPeer[]>('/api/v1/p2p/peers'),
  /** k-bucket 摘要 + 观测端点簿（GET /api/v1/p2p/buckets）。 */
  p2pBuckets: (): Promise<P2pBucketsResp> => get<P2pBucketsResp>('/api/v1/p2p/buckets'),
  /** 连接阶梯统计（GET /api/v1/p2p/ladder）。 */
  p2pLadder: (): Promise<P2pLadderStats> => get<P2pLadderStats>('/api/v1/p2p/ladder'),
  /** 身份冲突观测（GET /api/v1/p2p/identity-conflicts）→ P2pIdentityConflict[]；同公钥多地址进入的本地警告（仅提示不阻断）。 */
  p2pIdentityConflicts: (): Promise<P2pIdentityConflict[]> =>
    get<P2pIdentityConflict[]>('/api/v1/p2p/identity-conflicts'),
  /**
   * 节点元数据注册表快照（GET /api/v1/p2p/node-meta，公开）——os-p2p meta
   * 组件观察面：所有连接过本节点的节点（地址历史/分数/存活状态/来源），按
   * 健康分降序、Inactive 殿后。节点发现页主用 combined 的富化字段，本端点
   * 留作调试/后续消费。
   */
  p2pNodeMeta: (): Promise<P2pNodeMetaEntry[]> =>
    get<P2pNodeMetaEntry[]>('/api/v1/p2p/node-meta'),
  /**
   * 手动触发元数据心跳（POST /api/v1/p2p/node-meta/:id/reactivate，公开开发期）
   * ——Inactive → Active{score:30} 并立即探测一次（活连接即成功，否则纯 TCP
   * connect）。返回 {ok, node_id, probed}：probed=true 探活成功（条目复活，
   * 回到活跃组）；false = 不可达或注册表无此节点。探测可耗时数秒，自带 30s
   * 期限避开统一 15s 截断。
   */
  p2pNodeMetaReactivate: (
    nodeId: string,
  ): Promise<{ ok: boolean; node_id: string; probed: boolean; note?: string }> => {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 30_000);
    return request<{ ok: boolean; node_id: string; probed: boolean; note?: string }>(
      `/api/v1/p2p/node-meta/${encodeURIComponent(nodeId)}/reactivate`,
      { method: 'POST', body: {}, signal: controller.signal },
    ).finally(() => clearTimeout(timer));
  },
  /** 发应用消息（POST /api/v1/p2p/send，需 admin；fire-and-forget，送达在对端体现）。 */
  p2pSend: (nodeId: string, text: string): Promise<{ ok: boolean; to: string }> =>
    post<{ ok: boolean; to: string }>('/api/v1/p2p/send', { node_id: nodeId, text }),
  /**
   * 主动连接阶梯（POST /api/v1/p2p/connect，需 admin）——直连 → 打洞 → 中继，
   * 返回实际路径 {ok, node_id, path:"direct"|"punched"|"relayed"}。
   * 打洞可耗时数秒（3 轮 × 800ms 重试），自带 30s 期限避开统一 15s 截断。
   */
  p2pConnect: (nodeId: string): Promise<{ ok: boolean; node_id: string; path: string }> => {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 30_000);
    return request<{ ok: boolean; node_id: string; path: string }>(
      '/api/v1/p2p/connect',
      { method: 'POST', body: { node_id: nodeId }, signal: controller.signal },
    ).finally(() => clearTimeout(timer));
  },
  /**
   * 手动添加节点（POST /api/v1/p2p/add-peer，需 admin）——按 ip:port 直拨
   * （无端口后端补默认 7070），成功即入路由表；重复添加已连地址短路成功。
   * 返回 {ok, node_id, addr, note?}（note="already-connected" = 已直连短路）。
   * 拨号+握手可耗时数秒（connect 超时 + 握手上限 5s），自带 30s 期限避开统一 15s 截断。
   */
  p2pAddPeer: (
    addr: string,
  ): Promise<{ ok: boolean; node_id: string; addr: string; note?: string }> => {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 30_000);
    return request<{ ok: boolean; node_id: string; addr: string; note?: string }>(
      '/api/v1/p2p/add-peer',
      { method: 'POST', body: { addr }, signal: controller.signal },
    ).finally(() => clearTimeout(timer));
  },

  // —— 系统自举 / Provisioning（PXE / ISO / SSH）——
  /** 统计概要（GET /api/v1/provisioning/stats）。 */
  provisioningStats: (): Promise<unknown> => get('/api/v1/provisioning/stats'),

  // PXE 子域（/api/v1/provisioning/pxe/*）
  /** 读取 PXE 配置（GET /api/v1/provisioning/pxe/config）。 */
  provisioningPxeConfig: (): Promise<unknown> =>
    get('/api/v1/provisioning/pxe/config'),
  /** 更新 PXE 配置（POST /api/v1/provisioning/pxe/config）。 */
  updateProvisioningPxeConfig: (body: unknown): Promise<unknown> =>
    post('/api/v1/provisioning/pxe/config', body),
  /** 启动条目列表（GET /api/v1/provisioning/pxe/boot-entries）。 */
  provisioningPxeBootEntries: (): Promise<unknown> =>
    get('/api/v1/provisioning/pxe/boot-entries'),
  /** 添加启动条目（POST /api/v1/provisioning/pxe/boot-entries）。 */
  addProvisioningPxeBootEntry: (body: unknown): Promise<unknown> =>
    post('/api/v1/provisioning/pxe/boot-entries', body),
  /** 删除启动条目（DELETE /api/v1/provisioning/pxe/boot-entries/:id）。 */
  deleteProvisioningPxeBootEntry: (id: string): Promise<unknown> =>
    del(`/api/v1/provisioning/pxe/boot-entries/${encodeURIComponent(id)}`),
  /** PXE 服务状态（GET /api/v1/provisioning/pxe/status）。 */
  provisioningPxeStatus: (): Promise<unknown> =>
    get('/api/v1/provisioning/pxe/status'),
  /** 启动 PXE 服务（POST /api/v1/provisioning/pxe/start）。 */
  startProvisioningPxe: (): Promise<unknown> =>
    post('/api/v1/provisioning/pxe/start', {}),
  /** 停止 PXE 服务（POST /api/v1/provisioning/pxe/stop）。 */
  stopProvisioningPxe: (): Promise<unknown> =>
    post('/api/v1/provisioning/pxe/stop', {}),

  // ISO 子域（/api/v1/provisioning/iso/tasks*）
  /** 列全部 ISO 构建任务（GET /api/v1/provisioning/iso/tasks）。 */
  provisioningIsoTasks: (): Promise<unknown> =>
    get('/api/v1/provisioning/iso/tasks'),
  /** 创建 ISO 构建任务（POST /api/v1/provisioning/iso/tasks）。 */
  createProvisioningIsoTask: (body: unknown): Promise<unknown> =>
    post('/api/v1/provisioning/iso/tasks', body),
  /** 删除 ISO 构建任务（DELETE /api/v1/provisioning/iso/tasks/:id）。 */
  deleteProvisioningIsoTask: (id: string): Promise<unknown> =>
    del(`/api/v1/provisioning/iso/tasks/${encodeURIComponent(id)}`),
  /** 查询单个 ISO 构建任务（GET /api/v1/provisioning/iso/tasks/:id，building 时含实时 step/progress/build_log）。 */
  getProvisioningIsoTask: (id: string): Promise<unknown> =>
    get(`/api/v1/provisioning/iso/tasks/${encodeURIComponent(id)}`),
  /** 启动 ISO 真实构建（POST /api/v1/provisioning/iso/tasks/:id/build，需 admin；
   *  经 os-iso XorrisoIsoBuilder spawn mksquashfs/xorriso 子进程，任务
   *  pending→building→completed/failed，经任务详情轮询进度与构建日志）。 */
  buildProvisioningIsoTask: (id: string): Promise<unknown> =>
    post(`/api/v1/provisioning/iso/tasks/${encodeURIComponent(id)}/build`, {}),

  // SSH 子域（/api/v1/provisioning/ssh/*）
  /** 列全部 SSH 部署目标（GET /api/v1/provisioning/ssh/targets）。 */
  provisioningSshTargets: (): Promise<unknown> =>
    get('/api/v1/provisioning/ssh/targets'),
  /** 添加 SSH 部署目标（POST /api/v1/provisioning/ssh/targets）。 */
  addProvisioningSshTarget: (body: unknown): Promise<unknown> =>
    post('/api/v1/provisioning/ssh/targets', body),
  /** 删除 SSH 部署目标（DELETE /api/v1/provisioning/ssh/targets/:id）。 */
  deleteProvisioningSshTarget: (id: string): Promise<unknown> =>
    del(`/api/v1/provisioning/ssh/targets/${encodeURIComponent(id)}`),
  /** 测试 SSH 连接（POST /api/v1/provisioning/ssh/targets/:id/test）。 */
  testProvisioningSshTarget: (id: string): Promise<unknown> =>
    post(`/api/v1/provisioning/ssh/targets/${encodeURIComponent(id)}/test`, {}),
  /** 发起部署（POST /api/v1/provisioning/ssh/deploy，需 admin；真实执行：
   *  逐文件 scp + 可选 ssh 远程命令，返回 pending 态任务，前端轮询详情取
   *  文件级结果与命令输出）。 */
  provisioningDeploy: (body: unknown): Promise<unknown> =>
    post('/api/v1/provisioning/ssh/deploy', body),
  /** 查询部署任务（GET /api/v1/provisioning/ssh/deploy/:id，需 admin）。 */
  getProvisioningDeploy: (id: string): Promise<unknown> =>
    get(`/api/v1/provisioning/ssh/deploy/${encodeURIComponent(id)}`),
  /** 部署任务列表（GET /api/v1/provisioning/ssh/deploys，需 admin，最新在前；
   *  含远程路径与命令输出故收紧为 admin 读）。 */
  provisioningDeployTasks: (): Promise<unknown> =>
    get('/api/v1/provisioning/ssh/deploys'),

  // 电源控制子域（/api/v1/provisioning/power/*）——PXE 装机流水线第一环：
  // 本机 BMC in-band + 远程 IPMI 2.0 设备（lanplus）+ RMCP+ 网段扫描 + WoL 魔术唤醒。
  // 后端 ipmitool 缺失/无 /dev/ipmi0 时 BMC 域明确降级（available:false），WoL 域不受影响。
  /** 本机 BMC 聚合状态（GET /api/v1/provisioning/power/bmc：chassis/SEL/MC 键值）。 */
  powerBmc: (): Promise<unknown> => get('/api/v1/provisioning/power/bmc'),
  /** 本机电源控制（POST /api/v1/provisioning/power/bmc/power，需 admin；action: on|off|cycle|soft）。 */
  powerBmcPower: (action: string): Promise<unknown> =>
    post('/api/v1/provisioning/power/bmc/power', { action }),
  /** 本机传感器表（GET /api/v1/provisioning/power/bmc/sensors，截 200 行）。 */
  powerBmcSensors: (): Promise<unknown> =>
    get('/api/v1/provisioning/power/bmc/sensors'),
  /** 远程 IPMI 设备列表（GET /api/v1/provisioning/power/ipmi/devices，密码脱敏）。 */
  powerIpmiDevices: (): Promise<unknown> =>
    get('/api/v1/provisioning/power/ipmi/devices'),
  /** 注册远程 IPMI 设备（POST /api/v1/provisioning/power/ipmi/devices，需 admin）。 */
  addPowerIpmiDevice: (body: unknown): Promise<unknown> =>
    post('/api/v1/provisioning/power/ipmi/devices', body),
  /** 删除远程 IPMI 设备（DELETE /api/v1/provisioning/power/ipmi/devices/:id，需 admin）。 */
  deletePowerIpmiDevice: (id: string): Promise<unknown> =>
    del(`/api/v1/provisioning/power/ipmi/devices/${encodeURIComponent(id)}`),
  /** 测试远程设备连通性（POST .../power/ipmi/devices/:id/test，需 admin；
   *  真实 ipmitool lanplus chassis status，10s 超时）。 */
  testPowerIpmiDevice: (id: string): Promise<unknown> =>
    post(`/api/v1/provisioning/power/ipmi/devices/${encodeURIComponent(id)}/test`, {}),
  /** 远程设备电源控制（POST .../power/ipmi/devices/:id/power，需 admin）。 */
  powerIpmiDevicePower: (id: string, action: string): Promise<unknown> =>
    post(`/api/v1/provisioning/power/ipmi/devices/${encodeURIComponent(id)}/power`, { action }),
  /** 远程设备实时状态（GET .../power/ipmi/devices/:id/status：电源开/关、故障灯）。 */
  powerIpmiDeviceStatus: (id: string): Promise<unknown> =>
    get(`/api/v1/provisioning/power/ipmi/devices/${encodeURIComponent(id)}/status`),
  /** 发起网段扫描（POST /api/v1/provisioning/power/ipmi/scan，需 admin；
   *  纯 Rust RMCP Presence Ping（UDP 623 免凭据），仅 /24~/32，202 返回后台任务）。 */
  startPowerIpmiScan: (body: unknown): Promise<unknown> =>
    post('/api/v1/provisioning/power/ipmi/scan', body),
  /** 扫描任务状态+结果（GET /api/v1/provisioning/power/ipmi/scan/:id；running 时轮询）。 */
  getPowerIpmiScan: (id: string): Promise<unknown> =>
    get(`/api/v1/provisioning/power/ipmi/scan/${encodeURIComponent(id)}`),
  /** WoL 目标列表（GET /api/v1/provisioning/power/wol/targets，SecureOn 脱敏）。 */
  powerWolTargets: (): Promise<unknown> =>
    get('/api/v1/provisioning/power/wol/targets'),
  /** 注册 WoL 目标（POST /api/v1/provisioning/power/wol/targets，需 admin）。 */
  addPowerWolTarget: (body: unknown): Promise<unknown> =>
    post('/api/v1/provisioning/power/wol/targets', body),
  /** 删除 WoL 目标（DELETE /api/v1/provisioning/power/wol/targets/:id，需 admin）。 */
  deletePowerWolTarget: (id: string): Promise<unknown> =>
    del(`/api/v1/provisioning/power/wol/targets/${encodeURIComponent(id)}`),
  /** 发送魔术包唤醒（POST /api/v1/provisioning/power/wol/wake，开发期公开；
   *  body: {name}（已注册目标，带 SecureOn）或 {mac}；广播 ×3 间隔 100ms）。 */
  wakePowerWol: (body: unknown): Promise<unknown> =>
    post('/api/v1/provisioning/power/wol/wake', body),
  /** 局域网邻居（GET /api/v1/provisioning/power/wol/arp：ip neigh 解析的 MAC↔IP）。 */
  powerWolArp: (): Promise<unknown> => get('/api/v1/provisioning/power/wol/arp'),

  // —— 媒体（影院/音乐/相册）——
  /** 媒体库列表（GET /api/v1/media/library[?type=video|music|photo]）。 */
  mediaLibrary: (type?: string): Promise<unknown> =>
    get(`/api/v1/media/library${type ? '?type=' + encodeURIComponent(type) : ''}`),
  /** 媒体库统计（GET /api/v1/media/stats）。 */
  mediaStats: (): Promise<unknown> => get('/api/v1/media/stats'),
  /** 触发媒体库扫描（POST /api/v1/media/scan，需 admin）。 */
  mediaScan: (): Promise<unknown> => post('/api/v1/media/scan', {}),
  /** 刮削单个视频文件（POST /api/v1/media/scrape，需 admin）→ 调 TMDB 搜索 + 存 SQLite。 */
  mediaScrape: (filePath: string, mediaType = 'movie'): Promise<unknown> =>
    post('/api/v1/media/scrape', { file_path: filePath, media_type: mediaType }),
  /** 刮削任务状态（GET /api/v1/media/scrape/status）。 */
  mediaScrapeStatus: (): Promise<unknown> => get('/api/v1/media/scrape/status'),
  /** 批量刮削所有未刮削视频（POST /api/v1/media/scrape/all，需 admin）。 */
  mediaScrapeAll: (): Promise<unknown> => post('/api/v1/media/scrape/all', {}),
  /** 刮削后的元数据列表（GET /api/v1/media/metadata，含海报/剧情/评分）。 */
  mediaMetadata: (): Promise<unknown> => get('/api/v1/media/metadata'),
  // —— AI 相册（Qwen3-VL 图片识别 → 标签/描述/场景）——
  /** AI 分析照片（POST /api/v1/media/photo/analyze，需 admin）→ body.file_path 为空时分析全部未分析照片。 */
  photoAnalyze: (filePath?: string): Promise<unknown> =>
    post('/api/v1/media/photo/analyze', filePath ? { file_path: filePath } : {}),
  /** 列全部照片 AI 元数据（GET /api/v1/media/photo/ai-metadata，含标签/场景/颜色）。 */
  photoAiMetadata: (): Promise<unknown> => get('/api/v1/media/photo/ai-metadata'),
  /** 语义搜索照片（GET /api/v1/media/photo/search?q=，按 tags/description/scene 模糊匹配）。 */
  photoSearch: (q: string): Promise<unknown> =>
    get(`/api/v1/media/photo/search?q=${encodeURIComponent(q)}`),
  /** 按场景分类统计（GET /api/v1/media/photo/categories，{scene,count}[]）。 */
  photoCategories: (): Promise<unknown> => get('/api/v1/media/photo/categories'),
  /** 照片分析任务状态（GET /api/v1/media/photo/analyze/:id）。 */
  photoAnalyzeStatus: (id: string): Promise<unknown> =>
    get(`/api/v1/media/photo/analyze/${encodeURIComponent(id)}`),

  // —— 媒体生成（模型管理「生成」区：sd-turbo 文生图 + 视频任务框架，
  //    handlers/media_gen.rs；写需系统 admin token，读公开）——
  /**
   * 文生图（POST /api/v1/media/image，需 admin；真实 sd-turbo spawn python，
   * 服务端超时 60s → 本客户端超时放宽到 120s，避开统一 15s 拦腰截断）。
   * 503 = 显存不足/探测不可用（error 文案已含"先停 LLM 实例"指引）；
   * 502 = 生成失败/超时（error 带 stderr 摘要）。
   */
  mediaImageGenerate: (body: {
    prompt: string;
    /** 64 的倍数，256..=1024；省略默认 768。 */
    width?: number;
    /** 64 的倍数，256..=1024；省略默认 432（默认值不受 64 倍数约束）。 */
    height?: number;
    /** 1..=8；省略默认 4。 */
    steps?: number;
  }): Promise<MediaImageGenResp> => {
    // request() 的统一 15s 计时器对显式传入的 signal 不生效，这里自带 120s 期限
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 120_000);
    return request<MediaImageGenResp>('/api/v1/media/image', {
      method: 'POST',
      body,
      signal: controller.signal,
    }).finally(() => clearTimeout(timer));
  },
  /** 最近 50 条生成记录（GET /api/v1/media/image/recent，公开；不含图）。 */
  mediaImageRecent: (): Promise<MediaImageRecentItem[]> =>
    get<MediaImageRecentItem[]>('/api/v1/media/image/recent'),
  /**
   * 创建视频生成任务（POST /api/v1/media/video，需 admin，202）。
   * 当前无可用后端：任务创建即 failed（error 附指引，诚实不假装排队）。
   */
  mediaVideoCreate: (body: {
    prompt: string;
    /** 1..=30；省略默认 5。 */
    duration_secs?: number;
    /** external（默认）/ local。 */
    backend?: 'external' | 'local';
  }): Promise<MediaVideoTask> => post<MediaVideoTask>('/api/v1/media/video', body),
  /** 视频任务列表（GET /api/v1/media/video/tasks，公开）。 */
  mediaVideoTasks: (): Promise<MediaVideoTask[]> =>
    get<MediaVideoTask[]>('/api/v1/media/video/tasks'),
  /** 单个视频任务详情（GET /api/v1/media/video/tasks/:id，公开）。 */
  mediaVideoTask: (id: string): Promise<MediaVideoTask> =>
    get<MediaVideoTask>(`/api/v1/media/video/tasks/${encodeURIComponent(id)}`),

  // —— 文件管理器 ——
  /** 列目录（GET /api/v1/files/list[?path=]）。path 为空映射到根（/tank）。 */
  filesList: (path?: string): Promise<unknown> =>
    get(
      `/api/v1/files/list${path ? '?path=' + encodeURIComponent(path) : ''}`,
    ),
  /** 单文件 stat（GET /api/v1/files/stat?path=）。 */
  filesStat: (path: string): Promise<unknown> =>
    get(`/api/v1/files/stat?path=${encodeURIComponent(path)}`),
  /**
   * 目录递归用量（GET /api/v1/files/usage?path=）→
   * { path, total_bytes, file_count, dir_count, partial }。
   * partial=true 表示后端超限截断，各数值应按 "≥" 下界展示。
   */
  filesUsage: (path?: string): Promise<unknown> =>
    get(`/api/v1/files/usage${path ? '?path=' + encodeURIComponent(path) : ''}`),
  /** 创建目录（POST /api/v1/files/mkdir，需 admin）。 */
  filesMkdir: (path: string): Promise<unknown> =>
    post('/api/v1/files/mkdir', { path }),
  /** 删除文件/目录（POST /api/v1/files/delete，需 admin）。 */
  filesDelete: (path: string): Promise<unknown> =>
    post('/api/v1/files/delete', { path }),
  /** 重命名/移动（POST /api/v1/files/rename，需 admin）。 */
  filesRename: (from: string, to: string): Promise<unknown> =>
    post('/api/v1/files/rename', { from, to }),

  /**
   * 上传单文件到目标目录（POST /api/v1/files/upload?path=，需 admin）。
   *
   * 传输形态：经网关 JSON 通道 base64 装载（multipart 穿不过网关，见
   * files.rs 模块注释）；目标目录后端自动创建；重名自动 -1/-2 后缀；
   * >2 GiB 后端 413（调用方可用 FILES_MAX_UPLOAD_BYTES 预检）。
   * XHR 实现——upload.onprogress 支持进度回调（fetch 无上传进度）；
   * 未设超时（大文件上传可远超统一 15s）。返回 {name,size_bytes,path}。
   */
  filesUpload: (
    path: string,
    file: File,
    onProgress?: (loadedBytes: number, totalBytes: number) => void,
  ): Promise<FilesUploadResp> =>
    new Promise<FilesUploadResp>((resolve, reject) => {
      void (async () => {
        let b64: string;
        try {
          b64 = await fileToBase64(file);
        } catch (e) {
          reject(e instanceof Error ? e : new ApiError(String(e), { path }));
          return;
        }
        const url = `/api/v1/files/upload?path=${encodeURIComponent(path)}`;
        const xhr = new XMLHttpRequest();
        xhr.open('POST', url);
        xhr.setRequestHeader('Content-Type', 'application/json');
        const token = getApiToken();
        if (token) xhr.setRequestHeader('Authorization', `Bearer ${token}`);
        if (onProgress) {
          xhr.upload.onprogress = (ev) => onProgress(ev.loaded, ev.total);
        }
        xhr.onerror = () =>
          reject(new ApiError('上传失败：网络错误', { path: url }));
        xhr.onabort = () => reject(new ApiError('上传已取消', { path: url }));
        xhr.onload = () => {
          if (xhr.status >= 200 && xhr.status < 300) {
            try {
              resolve(JSON.parse(xhr.responseText) as FilesUploadResp);
            } catch {
              reject(
                new ApiError('上传响应解析失败', { status: xhr.status, path: url }),
              );
            }
            return;
          }
          let detail = xhr.responseText;
          try {
            const b = JSON.parse(xhr.responseText) as { error?: string };
            if (b && b.error) detail = b.error;
          } catch {
            /* 保留原文 */
          }
          reject(
            new ApiError(`上传失败：${xhr.status}${detail ? ' — ' + detail : ''}`, {
              status: xhr.status,
              path: url,
            }),
          );
        };
        xhr.send(JSON.stringify({ filename: file.name, content_base64: b64 }));
      })();
    }),

  /**
   * 下载文件并触发浏览器另存（GET /api/v1/files/download?path=，公开）。
   *
   * 传输形态：JSON 信封 content_base64 → 解码 Blob → ObjectURL →
   * `<a download>` 点击另存（window.open 直链拿到的是 JSON 信封而非字节流，
   * 网关响应恒 JSON 序列化所致，见 files.rs 模块注释）。超时放宽 120s
   * （显式 signal 使统一 15s 计时器失效）；目录后端 400；>2 GiB 后端 413。
   */
  filesDownload: async (path: string): Promise<void> => {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), 120_000);
    let resp: FilesDownloadResp;
    try {
      resp = await request<FilesDownloadResp>(
        `/api/v1/files/download?path=${encodeURIComponent(path)}`,
        { method: 'GET', signal: controller.signal },
      );
    } finally {
      clearTimeout(timer);
    }
    const blob = new Blob([base64ToBytes(resp.content_base64)], {
      type: resp.mime_type || 'application/octet-stream',
    });
    const objUrl = URL.createObjectURL(blob);
    try {
      const a = document.createElement('a');
      a.href = objUrl;
      a.download = resp.name || 'download';
      document.body.appendChild(a);
      a.click();
      a.remove();
    } finally {
      // 等浏览器取完数据再回收（立即 revoke 在部分浏览器会截断下载）
      setTimeout(() => URL.revokeObjectURL(objUrl), 10_000);
    }
  },

  // —— 下载中心 ——
  /** 列全部下载任务（GET /api/v1/downloads/tasks）。 */
  downloadTasks: (): Promise<unknown> => get('/api/v1/downloads/tasks'),
  /** 创建下载任务（POST /api/v1/downloads/tasks，需 admin）。savePath 必填（服务端 DTO 强制，缺省报 missing field save_path）。
   *  url 支持 HTTP/FTP/SFTP 直链、磁力链 magnet:?xt=urn:btih:、ed2k:// 及服务器本地 .torrent 路径。 */
  createDownload: (
    url: string,
    savePath: string,
    name?: string,
  ): Promise<unknown> =>
    post('/api/v1/downloads/tasks', { url, save_path: savePath, name }),
  /** 上传 .torrent 文件创建下载任务（POST /api/v1/downloads/torrent，需 admin；
   *  base64-JSON 信封，同 files.rs 惯例——网关契约无 multipart）。 */
  uploadTorrentDownload: (
    filename: string,
    contentBase64: string,
    savePath: string,
    name?: string,
  ): Promise<unknown> =>
    post('/api/v1/downloads/torrent', {
      filename,
      content_base64: contentBase64,
      save_path: savePath,
      name,
    }),
  /** 暂停任务（POST /api/v1/downloads/tasks/:id/pause，需 admin）。 */
  pauseDownload: (id: string): Promise<unknown> =>
    post(`/api/v1/downloads/tasks/${encodeURIComponent(id)}/pause`, {}),
  /** 继续任务（POST /api/v1/downloads/tasks/:id/resume，需 admin）。 */
  resumeDownload: (id: string): Promise<unknown> =>
    post(`/api/v1/downloads/tasks/${encodeURIComponent(id)}/resume`, {}),
  /** 取消任务（POST /api/v1/downloads/tasks/:id/cancel，需 admin）。 */
  cancelDownload: (id: string): Promise<unknown> =>
    post(`/api/v1/downloads/tasks/${encodeURIComponent(id)}/cancel`, {}),
  /** 删除任务（DELETE /api/v1/downloads/tasks/:id，需 admin）。 */
  deleteDownload: (id: string): Promise<unknown> =>
    del(`/api/v1/downloads/tasks/${encodeURIComponent(id)}`),
  /** 下载统计（GET /api/v1/downloads/stats）。 */
  downloadStats: (): Promise<unknown> => get('/api/v1/downloads/stats'),

  // —— P2P 传输（transfer 组件：经 os-p2p 叠加层网状分发，不依赖公网 IP）——
  /** 列 P2P 传输任务（GET /api/v1/transfer/tasks，公开）。进度=块位图，phase:
   *  querying/downloading/paused/completed/failed/cancelled（status 词与 downloads 对齐）。 */
  transferTasks: (): Promise<unknown> => get('/api/v1/transfer/tasks'),
  /** 单任务详情（GET /api/v1/transfer/tasks/:id，公开）。 */
  transferTask: (id: string): Promise<unknown> =>
    get(`/api/v1/transfer/tasks/${encodeURIComponent(id)}`),
  /** 发布本地文件为可传输（POST /api/v1/transfer/publish，需 admin）。
   *  返回 {transfer_id, sha256, ...}——把 sha256 发给其他节点的用户即可在其节点拉取。 */
  transferPublish: (path: string, name?: string): Promise<unknown> =>
    post('/api/v1/transfer/publish', { path, name }),
  /** 本机已发布清单（GET /api/v1/transfer/manifests，公开）。 */
  transferManifests: (): Promise<unknown> => get('/api/v1/transfer/manifests'),
  /** 下架清单（DELETE /api/v1/transfer/manifests/:id，需 admin；:id = transfer_id 或 sha256）。 */
  transferUnpublish: (id: string): Promise<unknown> =>
    del(`/api/v1/transfer/manifests/${encodeURIComponent(id)}`),
  /** 发起 P2P 拉取（POST /api/v1/transfer/fetch，需 admin）。
   *  key 为 64 hex sha256 或 tr_ 前缀 transfer_id；返回 202 + 任务视图（进度经 tasks 轮询）。 */
  transferFetch: (key: string, name?: string): Promise<unknown> =>
    post('/api/v1/transfer/fetch', key.startsWith('tr_')
      ? { transfer_id: key, name }
      : { sha256: key, name }),
  /** 暂停传输任务（POST /api/v1/transfer/tasks/:id/pause，需 admin；保留进度可续传）。 */
  transferPause: (id: string): Promise<unknown> =>
    post(`/api/v1/transfer/tasks/${encodeURIComponent(id)}/pause`, {}),
  /** 继续传输任务（POST /api/v1/transfer/tasks/:id/resume，需 admin）。 */
  transferResume: (id: string): Promise<unknown> =>
    post(`/api/v1/transfer/tasks/${encodeURIComponent(id)}/resume`, {}),
  /** 取消传输任务（POST /api/v1/transfer/tasks/:id/cancel，需 admin；进度文件保留）。 */
  transferCancel: (id: string): Promise<unknown> =>
    post(`/api/v1/transfer/tasks/${encodeURIComponent(id)}/cancel`, {}),
  /** 传输统计（GET /api/v1/transfer/stats，公开：清单数/任务数/做种贡献）。 */
  transferStats: (): Promise<unknown> => get('/api/v1/transfer/stats'),

  // —— 容器管理 ——
  /** 列容器（GET /api/v1/containers/list）。 */
  containerList: (): Promise<unknown> => get('/api/v1/containers/list'),
  /** 创建容器（POST /api/v1/containers/create，需 admin）。 */
  createContainer: (name: string, image: string): Promise<unknown> =>
    post('/api/v1/containers/create', { name, image }),
  /** 启动容器（POST /api/v1/containers/:id/start，需 admin）。 */
  startContainer: (id: string): Promise<unknown> =>
    post(`/api/v1/containers/${encodeURIComponent(id)}/start`, {}),
  /** 停止容器（POST /api/v1/containers/:id/stop，需 admin）。 */
  stopContainer: (id: string): Promise<unknown> =>
    post(`/api/v1/containers/${encodeURIComponent(id)}/stop`, {}),
  /** 重启容器（POST /api/v1/containers/:id/restart，需 admin）。 */
  restartContainer: (id: string): Promise<unknown> =>
    post(`/api/v1/containers/${encodeURIComponent(id)}/restart`, {}),
  /** 删除容器（DELETE /api/v1/containers/:id，需 admin）。 */
  deleteContainer: (id: string): Promise<unknown> =>
    del(`/api/v1/containers/${encodeURIComponent(id)}`),
  /** 列镜像（GET /api/v1/containers/images）。 */
  containerImages: (): Promise<unknown> => get('/api/v1/containers/images'),
  /** 容器统计（GET /api/v1/containers/stats）。 */
  containerStats: (): Promise<unknown> => get('/api/v1/containers/stats'),

  // —— 监控摄像头（RTSP/ONVIF 真实拉流 + 录像）——
  /** 列全部摄像头（GET /api/v1/surveillance/cameras）。 */
  surveillanceCameras: (): Promise<unknown> => get('/api/v1/surveillance/cameras'),
  /** 添加摄像头（POST /api/v1/surveillance/cameras，需 admin）。body: {name, url, protocol?} */
  addCamera: (body: unknown): Promise<unknown> =>
    post('/api/v1/surveillance/cameras', body),
  /** 删除摄像头（DELETE /api/v1/surveillance/cameras/:id，需 admin，停录像+拉流）。 */
  deleteCamera: (id: string): Promise<unknown> =>
    del(`/api/v1/surveillance/cameras/${encodeURIComponent(id)}`),
  /** 探测摄像头是否在线（POST /api/v1/surveillance/cameras/:id/probe，需 admin）。 */
  probeCamera: (id: string): Promise<unknown> =>
    post(`/api/v1/surveillance/cameras/${encodeURIComponent(id)}/probe`, {}),
  /** 启动实时转码 RTSP→HLS（POST /api/v1/surveillance/cameras/:id/stream，需 admin）。 */
  startStream: (id: string): Promise<unknown> =>
    post(`/api/v1/surveillance/cameras/${encodeURIComponent(id)}/stream`, {}),
  /** 停止实时转码（POST /api/v1/surveillance/cameras/:id/stop-stream，需 admin）。 */
  stopStream: (id: string): Promise<unknown> =>
    post(`/api/v1/surveillance/cameras/${encodeURIComponent(id)}/stop-stream`, {}),
  /** 开始录像 RTSP→MP4（POST /api/v1/surveillance/cameras/:id/record，需 admin）。 */
  startRecord: (id: string): Promise<unknown> =>
    post(`/api/v1/surveillance/cameras/${encodeURIComponent(id)}/record`, {}),
  /** 停止录像（POST /api/v1/surveillance/cameras/:id/stop-record，需 admin）。 */
  stopRecord: (id: string): Promise<unknown> =>
    post(`/api/v1/surveillance/cameras/${encodeURIComponent(id)}/stop-record`, {}),
  /** 列录像文件（GET /api/v1/surveillance/cameras/:id/recordings，含旧路径存量）。 */
  cameraRecordings: (id: string): Promise<unknown> =>
    get(`/api/v1/surveillance/cameras/${encodeURIComponent(id)}/recordings`),
  /** 摄像头统计（GET /api/v1/surveillance/stats，storage 为真实目录占用）。 */
  surveillanceStats: (): Promise<unknown> => get('/api/v1/surveillance/stats'),
  /** 网段扫描发现摄像头（POST /api/v1/surveillance/scan，需 admin）。body: {subnet?}，缺省取本机主网卡网段 */
  surveillanceScan: (body: { subnet?: string }): Promise<unknown> =>
    post('/api/v1/surveillance/scan', body),
  /** 读全局设置（GET /api/v1/surveillance/settings：recording_dir + 可写性 + 占用概览）。 */
  surveillanceSettings: (): Promise<unknown> => get('/api/v1/surveillance/settings'),
  /** 改录像根目录（POST /api/v1/surveillance/settings，需 admin）。body: {recording_dir}；只影响新录像 */
  updateSurveillanceSettings: (recordingDir: string): Promise<unknown> =>
    post('/api/v1/surveillance/settings', { recording_dir: recordingDir }),
  /** 批量添加摄像头（POST /api/v1/surveillance/cameras/batch，需 admin）。
   * body: {items:[{ip?,rtsp_url,vendor?}], username?, password?, name_prefix?}；逐台反馈成败 */
  addCamerasBatch: (body: {
    items: { ip?: string; rtsp_url: string; vendor?: string }[];
    username?: string;
    password?: string;
    name_prefix?: string;
  }): Promise<unknown> => post('/api/v1/surveillance/cameras/batch', body),
  /** 抓单帧快照（POST /api/v1/surveillance/cameras/:id/snapshot，需 admin，ffmpeg ≤8s）。返回 {data_url,...} */
  cameraSnapshot: (id: string): Promise<unknown> =>
    post(`/api/v1/surveillance/cameras/${encodeURIComponent(id)}/snapshot`, {}),
  /** 看最近快照（GET /api/v1/surveillance/cameras/:id/snapshot）。无快照 404 */
  cameraSnapshotLatest: (id: string): Promise<unknown> =>
    get(`/api/v1/surveillance/cameras/${encodeURIComponent(id)}/snapshot`),

  // —— 云同步 ——
  /** 列全部同步任务（GET /api/v1/cloudsync/tasks）。 */
  syncTasks: (): Promise<unknown> => get('/api/v1/cloudsync/tasks'),
  /** 创建同步任务（POST /api/v1/cloudsync/tasks，需 admin）。 */
  createSyncTask: (body: unknown): Promise<unknown> =>
    post('/api/v1/cloudsync/tasks', body),
  /** 触发同步（POST /api/v1/cloudsync/tasks/:id/sync，需 admin）。 */
  triggerSync: (id: string): Promise<unknown> =>
    post(`/api/v1/cloudsync/tasks/${encodeURIComponent(id)}/sync`, {}),
  /** 暂停同步（POST /api/v1/cloudsync/tasks/:id/pause，需 admin）。 */
  pauseSync: (id: string): Promise<unknown> =>
    post(`/api/v1/cloudsync/tasks/${encodeURIComponent(id)}/pause`, {}),
  /** 继续同步（POST /api/v1/cloudsync/tasks/:id/resume，需 admin）。 */
  resumeSync: (id: string): Promise<unknown> =>
    post(`/api/v1/cloudsync/tasks/${encodeURIComponent(id)}/resume`, {}),
  /** 删除同步任务（DELETE /api/v1/cloudsync/tasks/:id，需 admin）。 */
  deleteSyncTask: (id: string): Promise<unknown> =>
    del(`/api/v1/cloudsync/tasks/${encodeURIComponent(id)}`),
  /** 云同步统计（GET /api/v1/cloudsync/stats）。 */
  syncStats: (): Promise<unknown> => get('/api/v1/cloudsync/stats'),

  // —— 笔记/文档 ——
  /** 列全部笔记摘要（GET /api/v1/notes，不含 content）。 */
  notesList: (): Promise<unknown> => get('/api/v1/notes'),
  /** 单条笔记（GET /api/v1/notes/:id，含 content）。 */
  getNote: (id: string): Promise<unknown> =>
    get(`/api/v1/notes/${encodeURIComponent(id)}`),
  /** 创建笔记（POST /api/v1/notes，需 admin）。 */
  createNote: (body: unknown): Promise<unknown> => post('/api/v1/notes', body),
  /** 更新笔记（PUT /api/v1/notes/:id，需 admin）。 */
  updateNote: (id: string, body: unknown): Promise<unknown> =>
    request(`/api/v1/notes/${encodeURIComponent(id)}`, { method: 'PUT', body }),
  /** 删除笔记（DELETE /api/v1/notes/:id，需 admin）。 */
  deleteNote: (id: string): Promise<unknown> =>
    del(`/api/v1/notes/${encodeURIComponent(id)}`),
  /** 笔记统计（GET /api/v1/notes/stats）。 */
  notesStats: (): Promise<unknown> => get('/api/v1/notes/stats'),


  // 流媒体中心（/api/v1/streaming/*）已剥离为独立应用包 apps/streaming（NexHub
  // nexos-app-streaming）：endpoints 段随应用迁入其 src/api.ts（宿主桥 api 原语
  // 调用）；「直播」Tab（/api/v1/live/*）一并随包，端点后端常开不门控。
  // —— 备份管理（ZFS 快照 + 备份任务）——
  /** 列全部备份任务（GET /api/v1/backup/tasks）。 */
  backupTasks: (): Promise<unknown> => get('/api/v1/backup/tasks'),
  /** 创建备份任务（POST /api/v1/backup/tasks，需 admin）。 */
  createBackupTask: (body: unknown): Promise<unknown> =>
    post('/api/v1/backup/tasks', body),
  /** 立即执行备份任务（POST /api/v1/backup/tasks/:id/run，需 admin）。 */
  runBackupTask: (id: string): Promise<unknown> =>
    post(`/api/v1/backup/tasks/${encodeURIComponent(id)}/run`, {}),
  /** 删除备份任务（DELETE /api/v1/backup/tasks/:id，需 admin）。 */
  deleteBackupTask: (id: string): Promise<unknown> =>
    del(`/api/v1/backup/tasks/${encodeURIComponent(id)}`),
  /** 列全部 ZFS 快照（GET /api/v1/backup/snapshots，真实 zfs list）。 */
  backupSnapshots: (): Promise<unknown> => get('/api/v1/backup/snapshots'),
  /** 创建 ZFS 快照（POST /api/v1/backup/snapshots，需 admin，真实 zfs snapshot）。 */
  createBackupSnapshot: (body: unknown): Promise<unknown> =>
    post('/api/v1/backup/snapshots', body),
  /** 删除 ZFS 快照（DELETE /api/v1/backup/snapshots/:name，需 admin，真实 zfs destroy）。 */
  deleteBackupSnapshot: (name: string): Promise<unknown> =>
    del(`/api/v1/backup/snapshots/${encodeURIComponent(name)}`),
  /** 备份统计（GET /api/v1/backup/stats）。 */
  backupStats: (): Promise<unknown> => get('/api/v1/backup/stats'),

  // —— 系统监控（实时指标 + 告警 + ZFS 池）——
  /** 系统指标（GET /api/v1/monitor/metrics，真实 /proc 读取）。 */
  monitorMetrics: (): Promise<unknown> => get('/api/v1/monitor/metrics'),
  /**
   * 实时网速（GET /api/v1/monitor/net-rate，两次 /proc/net/dev 采样差值）：
   * `{total: {rx_bps, tx_bps}, interfaces: [{iface, rx_bps, tx_bps}]}`（bps =
   * 字节/秒，已排除 lo；首次调用全 0 记基线，下一轮差值生效）。
   */
  monitorNetRate: (): Promise<NetRateSnapshot> => get('/api/v1/monitor/net-rate'),
  /** 服务状态（GET /api/v1/monitor/services）。 */
  monitorServices: (): Promise<unknown> => get('/api/v1/monitor/services'),
  /** 告警列表（GET /api/v1/monitor/alerts）。 */
  monitorAlerts: (): Promise<unknown> => get('/api/v1/monitor/alerts'),
  /** 确认告警（POST /api/v1/monitor/alerts/:id/ack，需 admin）。 */
  ackMonitorAlert: (id: string): Promise<unknown> =>
    post(`/api/v1/monitor/alerts/${encodeURIComponent(id)}/ack`, {}),
  /** 历史采样（GET /api/v1/monitor/history）。 */
  monitorHistory: (): Promise<unknown> => get('/api/v1/monitor/history'),
  /** ZFS 池状态（GET /api/v1/monitor/zpools，真实 zpool list）。 */
  monitorZpools: (): Promise<unknown> => get('/api/v1/monitor/zpools'),
  /** 监控统计摘要（GET /api/v1/monitor/stats）。 */
  monitorStats: (): Promise<unknown> => get('/api/v1/monitor/stats'),

  // —— 模型管理（vLLM 推理）——
  /** GPU 信息（GET /api/v1/llm/gpu，动态探测 nvidia-smi/rocm-smi）。 */
  llmGpu: (): Promise<unknown> => get('/api/v1/llm/gpu'),
  /** 列全部推理实例（GET /api/v1/llm/instances）。 */
  llmInstances: (): Promise<unknown> => get('/api/v1/llm/instances'),
  /** 创建+启动推理实例（POST /api/v1/llm/instances，需 admin）。 */
  createLlmInstance: (body: unknown): Promise<unknown> =>
    post('/api/v1/llm/instances', body),
  /** 单实例详情（GET /api/v1/llm/instances/:id）。 */
  getLlmInstance: (id: string): Promise<unknown> =>
    get(`/api/v1/llm/instances/${encodeURIComponent(id)}`),
  /** 启动实例（POST /api/v1/llm/instances/:id/start，需 admin）。 */
  startLlmInstance: (id: string): Promise<unknown> =>
    post(`/api/v1/llm/instances/${encodeURIComponent(id)}/start`, {}),
  /** 停止实例（POST /api/v1/llm/instances/:id/stop，需 admin）。 */
  stopLlmInstance: (id: string): Promise<unknown> =>
    post(`/api/v1/llm/instances/${encodeURIComponent(id)}/stop`, {}),
  /** 删除实例（DELETE /api/v1/llm/instances/:id，需 admin）。 */
  deleteLlmInstance: (id: string): Promise<unknown> =>
    del(`/api/v1/llm/instances/${encodeURIComponent(id)}`),
  /** 健康探测（POST /api/v1/llm/instances/:id/health，需 admin）。 */
  checkLlmHealth: (id: string): Promise<unknown> =>
    post(`/api/v1/llm/instances/${encodeURIComponent(id)}/health`, {}),
  // 注：POST /api/v1/llm/instances/:id/chat（推理测试非流式）前端封装已随
  // 「推理测试」Tab 移除而删除（2026-09-02 统一「对话」Tab 直连实例端口
  // SSE 流式覆盖该能力；后端端点保留，供 AI/脚本调用）。
  /** 模型管理统计（GET /api/v1/llm/stats）。 */
  llmStats: (): Promise<unknown> => get('/api/v1/llm/stats'),
  /**
   * 实例轻量监控指标（GET /api/v1/llm/instances/:id/metrics，公开读）。
   *
   * 状态语义（消费方按此分支渲染，勿把离线当错误）：
   * - 404               → 实例不存在（真错误，reject）。
   * - reachable:false && simulated:false && metrics:null → 实例离线
   *   （vLLM /metrics 不可达；200 语义，监控探测不是错误）。
   * - simulated:true    → 合成模拟数据（NEXOS_LLM_METRICS_SIMULATE=1 且真实
   *   端口不通时后端返回；此时 reachable 可能为 false 但 metrics 非 null，
   *   UI 必须打「模拟」角标）。
   * - reachable:true    → 真实采集成功。
   *
   * null 容忍：metrics 内所有字段均为可空——Counter 速率三字段
   * （*_per_sec）首次采样无历史差值为 null（需 ≥5s 后第二次采样），
   * Gauge 字段随 vLLM 版本差异也可能缺失；UI 对 null 显示占位符。
   */
  llmInstanceMetrics: (id: string): Promise<unknown> =>
    get(`/api/v1/llm/instances/${encodeURIComponent(id)}/metrics`),
  /**
   * 实例拉起日志尾（GET /api/v1/llm/instances/:id/log?tail=200&follow=0，公开读）。
   *
   * 日志文件为按实例的 `<NEXOS_LLM_SPAWN_DIR>/llm-vllm-<id>.log`（stdout+stderr
   * 同文件）；follow 参数当前为拉取式实现——持续跟随由前端 2s 轮询完成。
   * 404 语义：实例不存在，或日志文件尚未生成（从未拉起过）。
   */
  llmInstanceLog: (id: string, opts?: { tail?: number; follow?: boolean }): Promise<LlmInstanceLog> => {
    const q = new URLSearchParams();
    if (opts?.tail != null && opts.tail > 0) q.set('tail', String(opts.tail));
    if (opts?.follow) q.set('follow', '1');
    const qs = q.toString();
    return get<LlmInstanceLog>(
      `/api/v1/llm/instances/${encodeURIComponent(id)}/log${qs ? `?${qs}` : ''}`,
    );
  },

  // —— vLLM Recipes 导入（配方库；handlers/llm.rs 烘焙代理，公开读）——
  // 浏览器直连 recipes.vllm.ai 会被 CORS 挡，外网请求统一由服务端代理（15s
  // 超时；上游失败 502 带原因）。服务端**常驻缓存（无 TTL）**：打开 Tab 只读
  // 缓存秒回零外呼；「刷新目录」按钮带 refresh=1 强制重拉并更新缓存。

  /**
   * GET /api/v1/llm/recipes/catalog 响应信封（2026-09-02 起）。
   * - `items`：上游 models.json 精简目录（同旧版裸数组元素形状）
   * - `cached_at`：本次缓存内容的采集时刻（RFC3339；null = 尚未拉取过）
   * - `from_cache`：true=进程缓存秒回 / false=刚从上游真实拉取
   */
  llmRecipesCatalog: (
    opts?: { refresh?: boolean },
  ): Promise<{ items: LlmRecipeCatalogItem[]; cached_at: string | null; from_cache: boolean }> =>
    get('/api/v1/llm/recipes/catalog' + (opts?.refresh ? '?refresh=1' : '')),
  /**
   * 单配方 JSON 原样透传（GET /api/v1/llm/recipes/recipe?hf_id=<HF模型ID>）。
   * 形状见 LlmRecipeDetail（上游宽松索引签名，缺字段容忍）；服务端常驻缓存
   * （随目录 refresh=1 一并清空）。
   */
  llmRecipe: (hfId: string): Promise<LlmRecipeDetail> =>
    get<LlmRecipeDetail>(`/api/v1/llm/recipes/recipe?hf_id=${encodeURIComponent(hfId)}`),
  /**
   * 网关可路由模型聚合（GET /api/v1/llm/gateway/models，公开读，真实探测）。
   *
   * 响应 `{gateway_visible: [...], unreachable: [...]}`；gateway_visible 条目含
   * `instance_id`（端口扫描发现的为 null）、`name`、`port`、`model_ids`、
   * `models`、`discovered`（实例表内 false / 扫描发现 true）。添加渠道对话框
   * 的「从本地发现导入」用它列可一键预填的本地 vLLM。
   */
  llmGatewayModels: (): Promise<unknown> => get('/api/v1/llm/gateway/models'),

  // —— 推理环境（vLLM Python venv 管理；handlers/llm_envs.rs，真实注册表数据）——
  /**
   * 环境列表（GET /api/v1/llm/environments，公开读）。
   *
   * 响应 `{environments: LlmEnvRow[], default_name: string | null}`；行字段见
   * LlmEnvRow（name/path/python_version/vllm_version_requested/
   * vllm_version_installed/is_default/status/size_bytes/created_at/updated_at/
   * last_error，status ∈ creating|updating|ready|error）。
   */
  llmEnvironments: (): Promise<{ environments: LlmEnvRow[]; default_name: string | null }> =>
    get('/api/v1/llm/environments'),
  /**
   * 创建推理环境（POST /api/v1/llm/environments，需 admin）→ 202 {task_id}。
   *
   * body：`{name, python_version?, vllm_version?, channel?}`（缺省 3.12 /
   * latest / stable）；channel='nightly' 为预置示例——恒装最新不钉版本
   * （vllm_version 被忽略），命令 `uv pip install -U vllm
   * --torch-backend=auto --extra-index-url https://wheels.vllm.ai/nightly`。
   * 后台任务跑 uv venv + uv pip install，轮询 llmEnvTask 看进度。
   */
  llmEnvCreate: (body: {
    name: string;
    python_version?: string;
    vllm_version?: string;
    channel?: string;
  }): Promise<unknown> => post('/api/v1/llm/environments', body),
  /**
   * 更新环境 vLLM 版本/渠道（POST /api/v1/llm/environments/:name/update，需
   * admin）→ 202 {task_id}（uv pip install -U；channel 可省=沿用当前渠道，
   * nightly↔stable 切换即重装）。
   */
  llmEnvUpdate: (
    name: string,
    vllm_version: string,
    channel?: string,
  ): Promise<unknown> =>
    post(`/api/v1/llm/environments/${encodeURIComponent(name)}/update`, {
      vllm_version,
      channel,
    }),
  /** 删除环境（DELETE /api/v1/llm/environments/:name，需 admin；默认环境 409）。 */
  llmEnvDelete: (name: string): Promise<unknown> =>
    del(`/api/v1/llm/environments/${encodeURIComponent(name)}`),
  /** 设为默认环境（POST /api/v1/llm/environments/:name/default，需 admin）。 */
  llmEnvSetDefault: (name: string): Promise<unknown> =>
    post(`/api/v1/llm/environments/${encodeURIComponent(name)}/default`, {}),
  /** 环境任务列表（GET /api/v1/llm/environments/tasks，公开读）。 */
  llmEnvTasks: (): Promise<{ tasks: LlmEnvTask[] }> =>
    get('/api/v1/llm/environments/tasks'),
  /**
   * 单环境任务详情（GET /api/v1/llm/environments/tasks/:id，公开读）。
   *
   * 含 `log: string[]`（环形上限 200 行，即日志尾）。
   */
  llmEnvTask: (id: string): Promise<LlmEnvTask> =>
    get(`/api/v1/llm/environments/tasks/${encodeURIComponent(id)}`),

  // —— 外部 API 接入（llm 组件子模块 handlers/llm_external.rs；2026-08-31）——
  /** 外部 API 登记列表（GET /api/v1/llm/external-apis，公开读；key 脱敏）。 */
  llmExternalApis: (): Promise<{ apis: LlmExternalApi[] }> =>
    get('/api/v1/llm/external-apis'),
  /**
   * 登记外部 API（POST /api/v1/llm/external-apis，需 admin）。
   *
   * body：`{name, base_url, api_key?, models?: string[], notes?}`（base_url 须
   * http(s)；models 可留空由连通测试回填）。
   */
  llmExternalApiCreate: (body: {
    name: string;
    base_url: string;
    api_key?: string;
    models?: string[];
    notes?: string;
    /** 来源 NodeID（联邦大厅一键导入自动写入，0x+66hex；非空走 overlay 中继）。 */
    via_node?: string;
  }): Promise<LlmExternalApi> => post('/api/v1/llm/external-apis', body),
  /**
   * 编辑登记（PUT /api/v1/llm/external-apis/:id，需 admin）。部分更新语义：
   * 未提供的字段保留原值；api_key/via_node 提供即覆盖（空串 = 清除）；校验
   * 同 POST（name 非空、base_url 须 http(s)、via_node 须 0x+66hex）。
   */
  llmExternalApiUpdate: (
    id: string,
    body: {
      name?: string;
      base_url?: string;
      api_key?: string;
      models?: string[];
      notes?: string;
      /** 来源 NodeID（空串 = 清除回直连语义）。 */
      via_node?: string;
    },
  ): Promise<LlmExternalApi> =>
    request(`/api/v1/llm/external-apis/${encodeURIComponent(id)}`, {
      method: 'PUT',
      body,
    }),
  /** 删除登记（DELETE /api/v1/llm/external-apis/:id，需 admin）。 */
  llmExternalApiDelete: (id: string): Promise<unknown> =>
    del(`/api/v1/llm/external-apis/${encodeURIComponent(id)}`),
  /**
   * 连通测试（POST /api/v1/llm/external-apis/:id/test，需 admin）：服务端真实
   * GET `<base_url>/models`（带鉴权头），返回真实模型清单 + 延迟。
   */
  llmExternalApiTest: (id: string): Promise<LlmExternalTestResult> =>
    post(`/api/v1/llm/external-apis/${encodeURIComponent(id)}/test`, {}),
  /**
   * 对话直通·非流式（POST /api/v1/llm/external-apis/:id/chat，需 admin）：转发
   * `<base_url>/chat/completions`，上游 JSON 原样透传（含 usage）。
   * `stream:true` 的 SSE 流式直通由 LlmModels.vue 裸 fetch（ReadableStream
   * 逐块渲染），不走本封装（request 会整包读 body，无法流式）。
   */
  llmExternalApiChat: (
    id: string,
    body: { model: string; messages: Array<{ role: string; content: string }>; max_tokens?: number },
  ): Promise<unknown> =>
    post(`/api/v1/llm/external-apis/${encodeURIComponent(id)}/chat`, body),

  // —— LLM API 网关（One API 风格：渠道聚合 / 令牌配额 / 代理转发 / 用量统计）——
  /** 列全部上游渠道（GET /api/v1/gateway/channels；via_node 非空 = 中继渠道）。 */
  gatewayChannels: (): Promise<unknown> => get('/api/v1/gateway/channels'),
  /**
   * 添加渠道（POST /api/v1/gateway/channels，需 admin）。
   *
   * body 字段：`{name, provider, base_url, api_key?, models?, priority?, weight?,
   * via_node?}` 或 `{from_discovery: {port, name?, models?}}` 或
   * `{from_external_api: <外部 API 登记 id>}`（2026-09-03 一键导入：后端复制
   * name/base_url/api_key/models/via_node——via_node 非空即中继渠道，models
   * 空则先探 `<base_url>/models` 回填；登记不存在 404）。
   */
  createGatewayChannel: (body: unknown): Promise<unknown> =>
    post('/api/v1/gateway/channels', body),
  /** 更新渠道（PUT /api/v1/gateway/channels/:id，需 admin）。 */
  updateGatewayChannel: (id: string, body: unknown): Promise<unknown> =>
    request(`/api/v1/gateway/channels/${encodeURIComponent(id)}`, { method: 'PUT', body }),
  /** 删除渠道（DELETE /api/v1/gateway/channels/:id，需 admin）。 */
  deleteGatewayChannel: (id: string): Promise<unknown> =>
    del(`/api/v1/gateway/channels/${encodeURIComponent(id)}`),
  /** 测试渠道连通性（POST /api/v1/gateway/channels/:id/test，需 admin）。 */
  testGatewayChannel: (id: string): Promise<unknown> =>
    post(`/api/v1/gateway/channels/${encodeURIComponent(id)}/test`, {}),
  /** 单渠道详情（GET /api/v1/gateway/channels/:id）。 */
  getGatewayChannel: (id: string): Promise<unknown> =>
    get(`/api/v1/gateway/channels/${encodeURIComponent(id)}`),
  /** 列全部令牌（GET /api/v1/gateway/tokens）。 */
  gatewayTokens: (): Promise<unknown> => get('/api/v1/gateway/tokens'),
  /**
   * 创建令牌（POST /api/v1/gateway/tokens，需 admin，自动生成 sk-os-xxx，201）。
   *
   * body 字段：
   * - `billing_mode`：计费模式 `"free"`（免费）| `"per_token"`（按 Token 计费，缺省）
   *   | `"per_image"`（按生成图片计费）| `"credits"`（积分制）
   * - `initial_credits`：初始积分（仅 credits 模式生效，服务端写入 quota_limit 作为积分池上限）
   * - 其余字段（name / quota_limit / allowed_models / expires_at）同旧版
   *
   * 响应 201 含 `billing_mode`、`quota_limit`、`quota_used` 及一次性明文 `key`。
   */
  createGatewayToken: (body: GatewayTokenCreateBody): Promise<unknown> =>
    post('/api/v1/gateway/tokens', body),
  /** 删除令牌（DELETE /api/v1/gateway/tokens/:id，需 admin）。 */
  deleteGatewayToken: (id: string): Promise<unknown> =>
    del(`/api/v1/gateway/tokens/${encodeURIComponent(id)}`),
  /** 禁用令牌（POST /api/v1/gateway/tokens/:id/disable，需 admin）。 */
  disableGatewayToken: (id: string): Promise<unknown> =>
    post(`/api/v1/gateway/tokens/${encodeURIComponent(id)}/disable`, {}),
  /** 启用令牌（POST /api/v1/gateway/tokens/:id/enable，需 admin）。 */
  enableGatewayToken: (id: string): Promise<unknown> =>
    post(`/api/v1/gateway/tokens/${encodeURIComponent(id)}/enable`, {}),
  /** 调用日志（GET /api/v1/gateway/logs?limit=，默认 50）。 */
  gatewayLogs: (limit?: number): Promise<unknown> =>
    get(`/api/v1/gateway/logs${limit ? `?limit=${limit}` : ''}`),
  /** 网关聚合统计（GET /api/v1/gateway/stats）。 */
  gatewayStats: (): Promise<unknown> => get('/api/v1/gateway/stats'),
  /** 聚合可用模型列表（GET /api/v1/gateway/models，去重）。 */
  gatewayModels: (): Promise<unknown> => get('/api/v1/gateway/models'),
  /**
   * OpenAI 形态模型列表（GET /api/v1/gateway/v1/models，对外接入端点，2026-08-31）。
   *
   * 鉴权用**网关令牌**（sk-os-，非管理端 token）：`Authorization: Bearer <sk-os-key>`
   * 覆盖全局管理 token（request 的 header 合入语义：调用方显式头最后合入）。
   * 响应 OpenAI list 契约：`{object:"list",data:[{id,object:"model",created,owned_by:"nexos-gateway"}]}`，
   * 已按令牌 allowed_models / allowed_channels 过滤（与实际可路由集合一致）。
   */
  gatewayV1Models: (apiKey: string): Promise<unknown> =>
    request('/api/v1/gateway/v1/models', {
      method: 'GET',
      headers: { Authorization: `Bearer ${apiKey.trim()}` },
    }),
  /** 列模型映射（GET /api/v1/gateway/mappings）。 */
  gatewayMappings: (): Promise<unknown> => get('/api/v1/gateway/mappings'),
  /** 添加模型映射（POST /api/v1/gateway/mappings，需 admin）。 */
  createGatewayMapping: (body: unknown): Promise<unknown> =>
    post('/api/v1/gateway/mappings', body),
  /** 删除模型映射（DELETE /api/v1/gateway/mappings/:name，需 admin）。 */
  deleteGatewayMapping: (name: string): Promise<unknown> =>
    del(`/api/v1/gateway/mappings/${encodeURIComponent(name)}`),

  // —— 充值订单（计费单位=积分；amount_crypto：usdt 两位小数 / btc 聪 / evm wei）——
  /**
   * 创建充值订单（POST /api/v1/gateway/payments，201）。
   *
   * body 字段：
   * - `token_id`：目标令牌 id（充值入该令牌的积分池）
   * - `currency`：`"usdt" | "btc" | "evm"`
   * - `credits`：要充值的积分数
   *
   * 响应 201：`{id, status:"pending", currency, amount_crypto, credits, address, memo, warning?}`。
   * `amount_crypto` 单位随币种：usdt 为两位小数的 USDT 数、btc 为聪（satoshi，1 BTC=1e8 聪）、
   * evm 为 wei（1 ETH=1e18 wei，memo 有说明）。服务端未配置收款地址 env 时 `address` 为空串
   * 且携带 `warning` 字段，需提示用户。
   */
  createGatewayPayment: (body: GatewayPaymentCreateBody): Promise<unknown> =>
    post('/api/v1/gateway/payments', body),
  /** 列充值订单（GET /api/v1/gateway/payments?status=，可按状态过滤，最新在前）。 */
  gatewayPayments: (status?: string): Promise<unknown> =>
    get(`/api/v1/gateway/payments${status ? `?status=${encodeURIComponent(status)}` : ''}`),
  /**
   * 确认到账（POST /api/v1/gateway/payments/:id/confirm，200）。
   * `txid` 可选（链上交易哈希，用于对账）。响应 `{ok, added_credits, order}`；
   * 对已确认订单重复调用返回 409（幂等冲突），调用方应捕获后按"已确认过"处理。
   */
  confirmGatewayPayment: (id: string, txid?: string): Promise<unknown> =>
    post(`/api/v1/gateway/payments/${encodeURIComponent(id)}/confirm`, txid ? { txid } : {}),
  /**
   * 拒绝订单（POST /api/v1/gateway/payments/:id/reject）。
   * `reason` 可选（拒绝原因，展示给对账方）。仅对 pending 订单有意义。
   */
  rejectGatewayPayment: (id: string, reason?: string): Promise<unknown> =>
    post(`/api/v1/gateway/payments/${encodeURIComponent(id)}/reject`, reason ? { reason } : {}),

  // —— 区块链管理（RPC 节点 + Blockscout 浏览器，4 类链编排）——
  /** 列全部 RPC 节点（GET /api/v1/blockchain/nodes）。 */
  blockchainNodes: (): Promise<unknown> => get('/api/v1/blockchain/nodes'),
  /** 创建节点（POST /api/v1/blockchain/nodes，需 admin）→ 构造 compose + start_cmd。 */
  createBlockchainNode: (body: unknown): Promise<unknown> =>
    post('/api/v1/blockchain/nodes', body),
  /** 单节点详情含 compose_yaml（GET /api/v1/blockchain/nodes/:id）。 */
  getBlockchainNode: (id: string): Promise<unknown> =>
    get(`/api/v1/blockchain/nodes/${encodeURIComponent(id)}`),
  /** 启动节点（POST /api/v1/blockchain/nodes/:id/start，需 admin，真实 spawn docker compose up -d）。 */
  startBlockchainNode: (id: string): Promise<unknown> =>
    post(`/api/v1/blockchain/nodes/${encodeURIComponent(id)}/start`, {}),
  /** 停止节点（POST /api/v1/blockchain/nodes/:id/stop，需 admin，真实 spawn docker compose down）。 */
  stopBlockchainNode: (id: string): Promise<unknown> =>
    post(`/api/v1/blockchain/nodes/${encodeURIComponent(id)}/stop`, {}),
  /** 删除节点（DELETE /api/v1/blockchain/nodes/:id，需 admin）。 */
  deleteBlockchainNode: (id: string): Promise<unknown> =>
    del(`/api/v1/blockchain/nodes/${encodeURIComponent(id)}`),
  /** 列全部区块链浏览器（GET /api/v1/blockchain/explorers）。 */
  blockchainExplorers: (): Promise<unknown> => get('/api/v1/blockchain/explorers'),
  /** 创建浏览器（POST /api/v1/blockchain/explorers，需 admin）→ 关联 node_id，构造 Blockscout compose。 */
  createBlockchainExplorer: (body: unknown): Promise<unknown> =>
    post('/api/v1/blockchain/explorers', body),
  /** 删除浏览器（DELETE /api/v1/blockchain/explorers/:id，需 admin）。 */
  deleteBlockchainExplorer: (id: string): Promise<unknown> =>
    del(`/api/v1/blockchain/explorers/${encodeURIComponent(id)}`),
  /** 启动浏览器（POST /api/v1/blockchain/explorers/:id/start，需 admin，真实 spawn docker compose up -d）。 */
  startBlockchainExplorer: (id: string): Promise<unknown> =>
    post(`/api/v1/blockchain/explorers/${encodeURIComponent(id)}/start`, {}),
  /** 4 类链预设配置（GET /api/v1/blockchain/chain-presets）。 */
  blockchainChainPresets: (): Promise<unknown> => get('/api/v1/blockchain/chain-presets'),
  /** 区块链统计（GET /api/v1/blockchain/stats）。 */
  blockchainStats: (): Promise<unknown> => get('/api/v1/blockchain/stats'),
  /** 支持的客户端列表 + 说明（GET /api/v1/blockchain/clients）。 */
  blockchainClients: (): Promise<unknown> => get('/api/v1/blockchain/clients'),

  // —— 节点运行管理（blockchain_nodes 子模块：geth/bitcoind 真实子进程，fast/full 模式）——
  /** 列节点（GET /api/v1/blockchain/chain-nodes，含运行状态修正）。 */
  chainNodes: (): Promise<unknown> => get('/api/v1/blockchain/chain-nodes'),
  /** 创建节点（POST /api/v1/blockchain/chain-nodes，需 admin；full 空间不足 → 409）。 */
  createChainNode: (body: unknown): Promise<unknown> =>
    post('/api/v1/blockchain/chain-nodes', body),
  /** 节点预设 + 预估体积 + 二进制探测（GET /api/v1/blockchain/chain-nodes/presets）。 */
  chainNodePresets: (): Promise<unknown> => get('/api/v1/blockchain/chain-nodes/presets'),
  /** 空间预检（GET /api/v1/blockchain/chain-nodes/space-check?kind=&network=&mode=&data_dir=&txindex=）。 */
  chainNodeSpaceCheck: (
    kind: string,
    network: string,
    mode: string,
    dataDir: string,
    txindex: boolean,
  ): Promise<unknown> =>
    get(
      `/api/v1/blockchain/chain-nodes/space-check?kind=${encodeURIComponent(kind)}` +
        `&network=${encodeURIComponent(network)}&mode=${encodeURIComponent(mode)}` +
        `&data_dir=${encodeURIComponent(dataDir)}&txindex=${txindex ? 1 : 0}`,
    ),
  /** 节点详情（GET /api/v1/blockchain/chain-nodes/:id）。 */
  getChainNode: (id: string): Promise<unknown> =>
    get(`/api/v1/blockchain/chain-nodes/${encodeURIComponent(id)}`),
  /** 启动节点（POST /api/v1/blockchain/chain-nodes/:id/start，需 admin；409=二进制缺失/空间不足）。 */
  startChainNode: (id: string): Promise<unknown> =>
    post(`/api/v1/blockchain/chain-nodes/${encodeURIComponent(id)}/start`, {}),
  /** 停止节点（POST /api/v1/blockchain/chain-nodes/:id/stop，需 admin）。 */
  stopChainNode: (id: string): Promise<unknown> =>
    post(`/api/v1/blockchain/chain-nodes/${encodeURIComponent(id)}/stop`, {}),
  /** 删除节点（DELETE /api/v1/blockchain/chain-nodes/:id，需 admin；链数据目录不自动删）。 */
  deleteChainNode: (id: string): Promise<unknown> =>
    del(`/api/v1/blockchain/chain-nodes/${encodeURIComponent(id)}`),
  /** 节点日志尾部（GET /api/v1/blockchain/chain-nodes/:id/logs?tail=200）。 */
  chainNodeLogs: (id: string, tail = 200): Promise<unknown> =>
    get(
      `/api/v1/blockchain/chain-nodes/${encodeURIComponent(id)}/logs?tail=${tail}`,
    ),

  // —— 模型仓库管理（本地模型库 + HF 缓存扫描 + modelscope 一键下载 + 推荐）——
  /**
   * 列本地模型（GET /api/v1/models/local）→ 自家模型库（/tank/models 等）+
   * HuggingFace hub 缓存（~/.cache/huggingface/hub，全用户 glob）合并清单：
   * [{ id, path, size_bytes, file_count, modified_at, has_config,
   *     source: 'local' | 'hf_cache', display_name }]。
   * HF 条目 id=org--name、display_name=org/name、path=snapshot 真实目录
   * （vLLM --model 可直接吃）；删除 HF 条目后端 400 拒（huggingface 工具链私有布局）。
   */
  modelLocal: (): Promise<unknown> => get('/api/v1/models/local'),
  /** 单模型详情（GET /api/v1/models/local/:id，文件列表 + config.json）。 */
  modelLocalDetail: (id: string): Promise<unknown> =>
    get(`/api/v1/models/local/${encodeURIComponent(id)}`),
  /** 删除本地模型（DELETE /api/v1/models/local/:id，需 admin，rm -rf 目录）。 */
  deleteModel: (id: string): Promise<unknown> =>
    del(`/api/v1/models/local/${encodeURIComponent(id)}`),
  /** 列下载任务（GET /api/v1/models/downloads）。 */
  modelDownloads: (): Promise<unknown> => get('/api/v1/models/downloads'),
  /** 创建下载任务（POST /api/v1/models/downloads，需 admin，spawn modelscope download）。 */
  createModelDownload: (modelId: string): Promise<unknown> =>
    post('/api/v1/models/downloads', { model_id: modelId }),
  /** 取消下载（DELETE /api/v1/models/downloads/:id，需 admin，kill pid）。 */
  cancelModelDownload: (id: string): Promise<unknown> =>
    del(`/api/v1/models/downloads/${encodeURIComponent(id)}`),
  /** 下载任务详情（GET /api/v1/models/downloads/:id，实时刷新进度）。 */
  modelDownloadDetail: (id: string): Promise<unknown> =>
    get(`/api/v1/models/downloads/${encodeURIComponent(id)}`),
  /** 推荐模型列表（GET /api/v1/models/recommended，标注 downloaded）。 */
  modelRecommended: (): Promise<unknown> => get('/api/v1/models/recommended'),
  /** 模型仓库统计（GET /api/v1/models/stats）。 */
  modelStats: (): Promise<unknown> => get('/api/v1/models/stats'),
  /**
   * 权重档案（GET /api/v1/models/:name/detail，A 面主端点）→
   * { name, path, total_size_bytes, file_count, complete,
   *   shards: { sharded, shard_total, shard_files, sequence_complete, missing_shards, index_file_present },
   *   config: { arch, num_hidden_layers, hidden_size, vocab_size, max_position_embeddings, raw } | null,
   *   files: [{ name, size_bytes, modified_at, shard_index, shard_total }] }。
   * complete：分片模型=序列连续+index.json 在场；单文件=有权重+config。400 非法名 / 404 不存在。
   */
  modelDetail: (name: string): Promise<unknown> =>
    get(`/api/v1/models/${encodeURIComponent(name)}/detail`),
  /**
   * 安全删除模型（DELETE /api/v1/models/:name，需 admin）。
   * 前置矩阵校验：name 含 `..`/`/`/`\`/NUL/`-` 开头 400；目标须为模型根直系目录（嵌套 400）；
   * 符号链接（导入产物）只解除链接、目标目录原样保留 → { ok, action: "unlink" }；
   * 真实目录 rm -rf → { ok, action: "delete" }。与 DELETE /local/:id 同一实现。
   */
  deleteModelByName: (name: string): Promise<unknown> =>
    del(`/api/v1/models/${encodeURIComponent(name)}`),
  /**
   * 导入库外模型目录（POST /api/v1/models/import，需 admin，符号链接导入不复制大文件）。
   * body: { path: "/home/oem/hf_models/Qwen3-VL-8B-Instruct" } → 201
   * { name, link_path, target_path }；源不存在 404 / 非模型目录 400 / 在库内 400 / 库内重名 409。
   */
  importModel: (path: string): Promise<unknown> => post('/api/v1/models/import', { path }),
  /**
   * 模型大厅列表（GET /api/v1/models/lobby[?name=&q=]）→ 同 name 多发布者合并为一条：
   * [{ name, display_name, description, tags, arch, size_bytes, file_count,
   *    download_count（各源求和）, sources: [{ sharer, source_url, size_bytes, file_count, created_at }],
   *    created_at, source_node?（联邦预留：本地='local'/缺省，远程=发布节点名——前端「联邦大厅」Tab 过滤依据） }],
   *    按 download_count 降序 + name 升序。
   * sources.length > 1 = 多人分享 = 多源并行下载可用。?name= 精确过滤；?q= 对
   * name/display_name/description/arch/tags 大小写不敏感子串匹配。
   */
  lobbyList: (opts?: { name?: string; q?: string }): Promise<unknown> => {
    const p = new URLSearchParams();
    if (opts?.name) p.set('name', opts.name);
    if (opts?.q) p.set('q', opts.q);
    const qs = p.toString();
    return get(`/api/v1/models/lobby${qs ? '?' + qs : ''}`);
  },
  /** 大厅单模型（GET /api/v1/models/lobby/:name）→ 与列表条目同结构的聚合 sources 单条；404 无此名。 */
  lobbyDetail: (name: string): Promise<unknown> =>
    get(`/api/v1/models/lobby/${encodeURIComponent(name)}`),
  /**
   * 发布本地模型到大厅（POST /api/v1/models/lobby/publish，需 admin，201）。
   * body: { name（必填，本地须存在，否则 404）, display_name?, description?, tags?, sharer? }
   * → { ok, id: "<name>@<sharer>", arch, size_bytes, file_count, sharer, source_url, share_token,
   *    created_at }。source_url 自动生成（含 admin token 的 share 基地址）；
   * 同 (name, sharer) 重复发布 = 刷新快照（INSERT OR REPLACE，保留 download_count）。
   */
  lobbyPublish: (body: {
    name: string;
    display_name?: string;
    description?: string;
    tags?: string[];
    sharer?: string;
  }): Promise<unknown> => post('/api/v1/models/lobby/publish', body),
  /**
   * 下架大厅条目（DELETE /api/v1/models/lobby/:id，需认证）。
   * id 形如 `<name>@<sharer>`；权限 admin 或同 sharer，否则 403；仅删条目不动本地模型。
   */
  lobbyUnpublish: (id: string): Promise<unknown> =>
    del(`/api/v1/models/lobby/${encodeURIComponent(id)}`),
  /**
   * 创建大厅多源下载任务（POST /api/v1/models/downloads，需 admin，201）。
   * body: { name, sources: [source_url, ...] }（来自大厅条目 sources；单源即长度 1，同一套代码）。
   * 后台文件级轮转分配（文件 i → 源 i % n）并行拉取，`.part` 断点续传 + 失败换源重试；
   * 全部源清单不可达 → 502 任务不入列。任务状态见 GET /downloads/:id（type=lobby_multi）。
   */
  createLobbyDownload: (name: string, sources: string[]): Promise<unknown> =>
    post('/api/v1/models/downloads', { name, sources }),
  /**
   * 探测在线仓库源（GET /api/v1/models/remote/:kind/:org/:model，公开读）。
   * kind='modelscope'（魔搭，默认 https://www.modelscope.cn）| 'hf'（HF 镜像，
   * 默认 https://hf-mirror.com）→ { ok, kind, repo_id, name, file_count,
   * total_size_bytes, files: [{ name, size_bytes, default_selected }] }。
   * 仓库不存在 404 / kind 非法 400 / 上游不可达 502。
   */
  probeModelRepo: (kind: 'modelscope' | 'hf', repoId: string): Promise<unknown> => {
    const [org, model] = repoId.trim().split('/');
    return get(
      `/api/v1/models/remote/${kind}/${encodeURIComponent(org)}/${encodeURIComponent(model)}`,
    );
  },
  /**
   * 创建在线仓库下载任务（POST /api/v1/models/remote/downloads，需 admin，201）。
   * body: { kind: 'modelscope'|'hf', repo_id: 'org/model', name?（本地目录名，缺省
   * repo 末段）, files?（勾选的相对路径数组，缺省=全部文件） }。
   * 后台逐文件 bounded Range（16 MiB/块）下载 + `.part` 断点续传 + 失败重试 3 次；
   * 任务混排在 GET /models/downloads（type='remote_repo'），进度同 lobby_multi 口径。
   */
  createModelRepoDownload: (body: {
    kind: 'modelscope' | 'hf';
    repo_id: string;
    name?: string;
    files?: string[];
  }): Promise<unknown> => post('/api/v1/models/remote/downloads', body),
  /**
   * Spark 专区（GET /api/v1/models/spark-zone，公开读，E 面）→
   * { ok, probed, origin: 'builtin'|'env', entries: [{ repo, org, quant, params, note,
   *   downloaded, sources: [{ kind: 'modelscope'|'hf', available, file_count,
   *   total_size_bytes, error }] }] }。
   * 策展清单为 SM120/NVFP4 优选（DGX Spark / RTX 50 系等通用，非 Spark 专属）；
   * probe=false（?probe=0）跳过两源实时探测（sources 恒"未探测"态）；
   * 失败源标 available=false 但条目不剔除。清单可经 env NEXOS_SPARK_ZONE_FILE 覆盖。
   */
  sparkZone: (opts?: { probe?: boolean }): Promise<unknown> =>
    get(`/api/v1/models/spark-zone${opts?.probe === false ? '?probe=0' : ''}`),

  // —— 应用中心（仅 NexOS 官方应用，source=nexos；无 apt/snap/flatpak 上架渠道）——
  /** 列商店应用（GET /api/v1/appstore/apps[?category=]，仅 nexos 来源）。 */
  appStoreApps: (category?: string): Promise<unknown> =>
    get(
      `/api/v1/appstore/apps${category ? '?category=' + encodeURIComponent(category) : ''}`,
    ),
  /** 单应用详情（GET /api/v1/appstore/apps/:id）。 */
  appStoreAppDetail: (id: string): Promise<unknown> =>
    get(`/api/v1/appstore/apps/${encodeURIComponent(id)}`),
  /** 分类列表含应用数（GET /api/v1/appstore/categories）。 */
  appStoreCategories: (): Promise<unknown> => get('/api/v1/appstore/categories'),
  /** 列已安装应用（GET /api/v1/appstore/installed，flatpak list 探测）。 */
  appStoreInstalled: (): Promise<unknown> => get('/api/v1/appstore/installed'),
  /** 安装应用（POST /api/v1/appstore/install，需 admin，内置模块即时完成）。 */
  appStoreInstall: (appId: string): Promise<unknown> =>
    post('/api/v1/appstore/install', { app_id: appId }),
  /** 卸载 flatpak 应用（POST /api/v1/appstore/uninstall，需 admin；nexos 内置应用拒绝）。 */
  appStoreUninstall: (appId: string, installType: string): Promise<unknown> =>
    post('/api/v1/appstore/uninstall', { app_id: appId, install_type: installType }),
  /** 列安装任务（GET /api/v1/appstore/tasks）。 */
  appStoreTasks: (): Promise<unknown> => get('/api/v1/appstore/tasks'),
  /** 安装任务详情含 log_tail（GET /api/v1/appstore/tasks/:id）。 */
  appStoreTaskDetail: (id: string): Promise<unknown> =>
    get(`/api/v1/appstore/tasks/${encodeURIComponent(id)}`),
  /** 发布应用（POST /api/v1/appstore/publish，需 admin，仅 nexos 渠道）。 */
  appStorePublish: (body: unknown): Promise<unknown> =>
    post('/api/v1/appstore/publish', body),
  /** 删除发布的应用（DELETE /api/v1/appstore/published/:id，需 admin）。 */
  deleteAppStorePublished: (id: string): Promise<unknown> =>
    del(`/api/v1/appstore/published/${encodeURIComponent(id)}`),
  /** 应用中心统计（GET /api/v1/appstore/stats）。 */
  appStoreStats: (): Promise<unknown> => get('/api/v1/appstore/stats'),

  // —— 应用包运行时（/api/v1/apps，docs/APPS.md；函数声明在文件尾部，自动提升）——
  /** 已安装应用包列表（GET /api/v1/apps）。 */
  appsList: (): Promise<AppsListResp> => appsList(),
  /** 应用包目录（GET /api/v1/apps/catalog）。 */
  appsCatalog: (): Promise<AppsCatalogResp> => appsCatalog(),
  /** 安装应用包（POST /api/v1/apps/install {repo}）。 */
  appsInstall: (repo: string): Promise<AppsInstallResp> => appsInstall(repo),
  /** 卸载应用包（DELETE /api/v1/apps/:id）。 */
  appsUninstall: (id: string): Promise<null> => appsUninstall(id),

  // —— Agent 集合（常用 AI coding agent 一键安装，docs/AGENT_HUB.md）——
  /** 列 agent 目录（GET /api/v1/agenthub/agents[?category=]，含 installed 探测）。 */
  agentHubAgents: (category?: string): Promise<unknown> =>
    get(
      `/api/v1/agenthub/agents${category ? '?category=' + encodeURIComponent(category) : ''}`,
    ),
  /** 单 agent 详情（GET /api/v1/agenthub/agents/:id）。 */
  agentHubAgentDetail: (id: string): Promise<unknown> =>
    get(`/api/v1/agenthub/agents/${encodeURIComponent(id)}`),
  /** 已安装列表（GET /api/v1/agenthub/installed，command -v 探测）。 */
  agentHubInstalled: (): Promise<unknown> => get('/api/v1/agenthub/installed'),
  /** 工具链可用性（GET /api/v1/agenthub/toolchains，node/npm/uv/cargo/curl）。 */
  agentHubToolchains: (): Promise<unknown> => get('/api/v1/agenthub/toolchains'),
  /** 一键安装（POST /api/v1/agenthub/install，需 admin，后台任务）。 */
  agentHubInstall: (agentId: string): Promise<unknown> =>
    post('/api/v1/agenthub/install', { agent_id: agentId }),
  /** 卸载（POST /api/v1/agenthub/uninstall，需 admin；script 渠道 400）。 */
  agentHubUninstall: (agentId: string): Promise<unknown> =>
    post('/api/v1/agenthub/uninstall', { agent_id: agentId }),
  /** 任务列表（GET /api/v1/agenthub/tasks）。 */
  agentHubTasks: (): Promise<unknown> => get('/api/v1/agenthub/tasks'),
  /** 任务详情含 log_tail（GET /api/v1/agenthub/tasks/:id）。 */
  agentHubTaskDetail: (id: string): Promise<unknown> =>
    get(`/api/v1/agenthub/tasks/${encodeURIComponent(id)}`),
  /** 发布自定义 agent（POST /api/v1/agenthub/publish，需 admin，持久化）。 */
  agentHubPublish: (body: {
    name: string;
    description?: string;
    install_type: string;
    install_target: string;
    check_binary: string;
    homepage?: string;
  }): Promise<unknown> => post('/api/v1/agenthub/publish', body),
  /** 删自定义 agent（DELETE /api/v1/agenthub/published/:id，需 admin，预置不可删）。 */
  deleteAgentHubPublished: (id: string): Promise<unknown> =>
    del(`/api/v1/agenthub/published/${encodeURIComponent(id)}`),
  /** Agent 集合统计（GET /api/v1/agenthub/stats）。 */
  agentHubStats: (): Promise<unknown> => get('/api/v1/agenthub/stats'),
  /**
   * 手动安装工具链（POST /api/v1/agenthub/toolchain/install，需 admin）。
   * name: "node"（覆盖 node+npm）/ "uv" / "cargo"；202 返回 {task_id, status}。
   */
  agentHubToolchainInstall: (name: string): Promise<unknown> =>
    post('/api/v1/agenthub/toolchain/install', { name }),
  /** 工具链安装任务详情含环形日志（GET /api/v1/agenthub/toolchain/install/tasks/:id）。 */
  agentHubToolchainInstallTask: (id: string): Promise<unknown> =>
    get(`/api/v1/agenthub/toolchain/install/tasks/${encodeURIComponent(id)}`),
  /**
   * 启动 agent Web 界面服务（POST /api/v1/agenthub/web/:agentId/start，需 admin）。
   * 仅 web 描述符标注的 agent（首期 OpenCode）；返回 {url, pid, port, state}，
   * state ∈ started | idempotent | recovered；前端拿 url 直接 window.open。
   */
  agentHubWebStart: (agentId: string): Promise<unknown> =>
    post(`/api/v1/agenthub/web/${encodeURIComponent(agentId)}/start`, {}),
  /** 停止 agent Web 界面服务（POST /api/v1/agenthub/web/:agentId/stop，需 admin）。 */
  agentHubWebStop: (agentId: string): Promise<unknown> =>
    post(`/api/v1/agenthub/web/${encodeURIComponent(agentId)}/stop`, {}),
  /**
   * agent Web 界面服务状态（GET /api/v1/agenthub/web/:agentId/status，公开）。
   * 返回 {running, url, pid, port, started_at, log_tail}；表丢失但端口在监听时
   * 后端按端口重建表（os-api 重启恢复）。
   */
  agentHubWebStatus: (agentId: string): Promise<unknown> =>
    get(`/api/v1/agenthub/web/${encodeURIComponent(agentId)}/status`),

  // 二维码传输（/api/v1/qr/*）已剥离为独立应用包 apps/qrtransfer（NexHub
  // nexos-app-qrtransfer）：endpoints 段随应用迁入其 src/api.ts（宿主桥 api
  // 原语调用）。BleHub 的 mesh 连接 QR 直连同端点（见 BleHub.vue）。

  // —— BLE mesh 网状中继枢纽（OS 作 mesh 节点 + 互联网网关，手机离线经 BLE mesh 多跳中继）——
  /** mesh Hub 状态（GET /api/v1/ble/status）→ { running, adapter, address, node_count, direct_connections, pid? }。 */
  bleStatus: (): Promise<unknown> => get('/api/v1/ble/status'),
  /** 启动 BLE mesh relay GATT 服务（POST /api/v1/ble/start，需 admin）→ spawn python 脚本。 */
  bleStart: (): Promise<unknown> => post('/api/v1/ble/start', {}),
  /** 停止 mesh relay（POST /api/v1/ble/stop，需 admin）→ kill pid。 */
  bleStop: (): Promise<unknown> => post('/api/v1/ble/stop', {}),
  /** 列 mesh 节点（GET /api/v1/ble/nodes）→ 直接 + 间接（含 hop 数）。 */
  bleNodes: (): Promise<unknown> => get('/api/v1/ble/nodes'),
  /** 移除 mesh 节点（DELETE /api/v1/ble/nodes/:id，需 admin）。 */
  deleteBleNode: (id: string): Promise<unknown> =>
    del(`/api/v1/ble/nodes/${encodeURIComponent(id)}`),
  /** 节点发现通告（POST /api/v1/ble/discover，内部 API）→ 手机上报 {node_id, name, reachable, direct}。 */
  bleDiscover: (body: unknown): Promise<unknown> => post('/api/v1/ble/discover', body),
  /** 路由表（GET /api/v1/ble/routing）→ { self, entries: [{node_id, hop, via, direct}] }。 */
  bleRouting: (): Promise<unknown> => get('/api/v1/ble/routing'),
  /** mesh 消息中继（POST /api/v1/ble/messages，内部 API，flooding + hop_count 去重）。 */
  bleRelayMessage: (body: unknown): Promise<unknown> => post('/api/v1/ble/messages', body),
  /** mesh 消息历史（GET /api/v1/ble/messages）。 */
  bleMessages: (): Promise<unknown> => get('/api/v1/ble/messages'),
  /** mesh 统计（GET /api/v1/ble/stats）→ { node_count, direct, reachable, message_count, running }。 */
  bleStats: (): Promise<unknown> => get('/api/v1/ble/stats'),

  // —— 代码仓库中心（原生 git 管理，零依赖：裸仓库 CRUD + 文件浏览 + 提交历史 + 目录导入）——
  /** 列仓库（GET /api/v1/coderepo/repos）→ { repos: [{ name, description, size_bytes, last_commit, branch_count, commit_count, clone_url_ssh, ... }] }。 */
  codeRepoRepos: (): Promise<unknown> => get('/api/v1/coderepo/repos'),
  /** 创建裸仓库（POST /api/v1/coderepo/repos，需 admin）body: { name, description? }。 */
  createCodeRepo: (body: { name: string; description?: string }): Promise<unknown> =>
    post('/api/v1/coderepo/repos', body),
  /** 删除仓库（DELETE /api/v1/coderepo/repos/:name，需 admin，rm -rf 裸仓库）。 */
  deleteCodeRepo: (name: string): Promise<unknown> =>
    del(`/api/v1/coderepo/repos/${encodeURIComponent(name)}`),
  /** 文件树 + 分支（GET /api/v1/coderepo/repos/:name/contents）→ { name, default_branch, branches: [...], tree: [...] }。 */
  codeRepoContents: (name: string): Promise<unknown> =>
    get(`/api/v1/coderepo/repos/${encodeURIComponent(name)}/contents`),
  /** 文件内容（GET /api/v1/coderepo/repos/:name/file?path=...）→ { name, path, ok, exists, content }。 */
  codeRepoFile: (name: string, path: string): Promise<unknown> =>
    get(`/api/v1/coderepo/repos/${encodeURIComponent(name)}/file?path=${encodeURIComponent(path)}`),
  /** 提交历史（GET /api/v1/coderepo/repos/:name/commits）→ { name, commits: [{ hash, author, message, date }] }。 */
  codeRepoCommits: (name: string): Promise<unknown> =>
    get(`/api/v1/coderepo/repos/${encodeURIComponent(name)}/commits`),
  /** 获取 clone URL（POST /api/v1/coderepo/repos/:name/clone-url，需 admin）→ { name, clone_url_ssh, clone_url_http }。 */
  codeRepoCloneUrl: (name: string): Promise<unknown> =>
    post(`/api/v1/coderepo/repos/${encodeURIComponent(name)}/clone-url`, {}),
  /** 导入现有目录为仓库（POST /api/v1/coderepo/repos/:name/import，需 admin）body: { source_dir } → git init + add + commit + push。 */
  codeRepoImport: (name: string, sourceDir: string): Promise<unknown> =>
    post(`/api/v1/coderepo/repos/${encodeURIComponent(name)}/import`, { source_dir: sourceDir }),
  /** 列 AI 会话记录（GET /api/v1/coderepo/sessions）。 */
  codeRepoSessions: (): Promise<unknown> => get('/api/v1/coderepo/sessions'),
  /** 创建会话记录（POST /api/v1/coderepo/sessions，需 admin）body: { agent_name, repo_name, summary?, files_changed?, commits? }。 */
  createCodeRepoSession: (body: unknown): Promise<unknown> =>
    post('/api/v1/coderepo/sessions', body),
  /** 结束会话（POST /api/v1/coderepo/sessions/:id/end，需 admin）。 */
  endCodeRepoSession: (id: string): Promise<unknown> =>
    post(`/api/v1/coderepo/sessions/${encodeURIComponent(id)}/end`, {}),
  /** 代码仓库统计（GET /api/v1/coderepo/stats）→ { repo_count, total_size, session_count, total_commits }。 */
  codeRepoStats: (): Promise<unknown> => get('/api/v1/coderepo/stats'),

  // —— 内置 CI（nexhub_ci 组件，v0.1.33：读公开 / 触发与清记录 admin）——
  /** 手动触发 CI（POST /api/v1/coderepo/repos/:name/ci，需 admin）→ 202 { ok, run }。 */
  codeRepoCiTrigger: (name: string): Promise<CiTriggerResp> =>
    post(`/api/v1/coderepo/repos/${encodeURIComponent(name)}/ci`, {}),
  /** 该仓 CI runs（GET /api/v1/coderepo/repos/:name/ci，最新 20，不含 log）。 */
  codeRepoCiRuns: (name: string): Promise<CiRunsResp> =>
    get(`/api/v1/coderepo/repos/${encodeURIComponent(name)}/ci`),
  /** run 详情 + 环形日志全文（GET .../ci/:runId）。 */
  codeRepoCiRun: (name: string, runId: string): Promise<CiRunResp> =>
    get(`/api/v1/coderepo/repos/${encodeURIComponent(name)}/ci/${encodeURIComponent(runId)}`),
  /** 删除 run 记录（DELETE .../ci/:runId，需 admin；queued/running 409）。 */
  codeRepoCiDeleteRun: (name: string, runId: string): Promise<{ ok: boolean; id: string }> =>
    del(`/api/v1/coderepo/repos/${encodeURIComponent(name)}/ci/${encodeURIComponent(runId)}`),
  /** 各仓最新 run 摘要聚合（GET /api/v1/coderepo/ci/latest）→ { latest: [...] }（仓库卡徽章数据源）。 */
  codeRepoCiLatest: (): Promise<CiLatestResp> => get('/api/v1/coderepo/ci/latest'),

  // —— 项目级 Issues / Pull Requests（coderepo 协作层，os-nexhub issues.rs）——
  // 读公开；写需身份（nexhub token / admin 回落，NexhubOpts 覆盖注入，
  // author 服务端反查，body 自报忽略）。
  /** Issue 列表（GET /api/v1/coderepo/repos/:name/issues?state=open|closed|all，公开）。 */
  codeRepoIssues: (name: string, state?: string): Promise<RepoIssuesResp> => {
    const p = new URLSearchParams();
    if (state) p.set('state', state);
    const qs = p.toString();
    return get(
      `/api/v1/coderepo/repos/${encodeURIComponent(name)}/issues${qs ? '?' + qs : ''}`,
    );
  },
  /** 建 Issue（POST .../issues，需身份；number 自动分配，labels 数组或逗号串均可）。 */
  createCodeRepoIssue: (
    name: string,
    body: { title: string; body?: string; labels?: string[] | string },
    opts?: NexhubOpts,
  ): Promise<{ ok: boolean; issue: RepoIssue }> =>
    request(`/api/v1/coderepo/repos/${encodeURIComponent(name)}/issues`, {
      method: 'POST',
      body,
      headers: nexhubAuthHeaders(opts),
    }),
  /** Issue 详情含评论流（GET .../issues/:num，公开）。 */
  codeRepoIssueDetail: (name: string, num: number): Promise<RepoIssueDetailResp> =>
    get(`/api/v1/coderepo/repos/${encodeURIComponent(name)}/issues/${num}`),
  /** Issue 评论（POST .../issues/:num/comments，需身份）。 */
  codeRepoIssueComment: (
    name: string,
    num: number,
    body: { body: string },
    opts?: NexhubOpts,
  ): Promise<{ ok: boolean; comment: RepoComment }> =>
    request(
      `/api/v1/coderepo/repos/${encodeURIComponent(name)}/issues/${num}/comments`,
      { method: 'POST', body, headers: nexhubAuthHeaders(opts) },
    ),
  /** 关闭 Issue（POST .../issues/:num/close；仅作者本人或 admin，否则 403）。 */
  codeRepoIssueClose: (name: string, num: number, opts?: NexhubOpts): Promise<unknown> =>
    request(
      `/api/v1/coderepo/repos/${encodeURIComponent(name)}/issues/${num}/close`,
      { method: 'POST', body: {}, headers: nexhubAuthHeaders(opts) },
    ),
  /** 重开 Issue（POST .../issues/:num/open；仅作者本人或 admin）。 */
  codeRepoIssueOpen: (name: string, num: number, opts?: NexhubOpts): Promise<unknown> =>
    request(
      `/api/v1/coderepo/repos/${encodeURIComponent(name)}/issues/${num}/open`,
      { method: 'POST', body: {}, headers: nexhubAuthHeaders(opts) },
    ),
  /** PR 列表（GET .../pulls?state=open|merged|closed|all，公开）。 */
  codeRepoPulls: (name: string, state?: string): Promise<RepoPullsResp> => {
    const p = new URLSearchParams();
    if (state) p.set('state', state);
    const qs = p.toString();
    return get(`/api/v1/coderepo/repos/${encodeURIComponent(name)}/pulls${qs ? '?' + qs : ''}`);
  },
  /** 建 PR（POST .../pulls，需身份；from_branch 须已 push 到裸仓，to_branch 缺省=仓库默认分支）。 */
  createCodeRepoPull: (
    name: string,
    body: { title: string; body?: string; from_branch: string; to_branch?: string },
    opts?: NexhubOpts,
  ): Promise<{ ok: boolean; pull: RepoPull }> =>
    request(`/api/v1/coderepo/repos/${encodeURIComponent(name)}/pulls`, {
      method: 'POST',
      body,
      headers: nexhubAuthHeaders(opts),
    }),
  /** PR 详情（GET .../pulls/:num，公开；含评论流 + diff_stat 摘要）。 */
  codeRepoPullDetail: (name: string, num: number): Promise<RepoPullDetailResp> =>
    get(`/api/v1/coderepo/repos/${encodeURIComponent(name)}/pulls/${num}`),
  /** PR 评论（POST .../pulls/:num/comments，需身份）。 */
  codeRepoPullComment: (
    name: string,
    num: number,
    body: { body: string },
    opts?: NexhubOpts,
  ): Promise<{ ok: boolean; comment: RepoComment }> =>
    request(
      `/api/v1/coderepo/repos/${encodeURIComponent(name)}/pulls/${num}/comments`,
      { method: 'POST', body, headers: nexhubAuthHeaders(opts) },
    ),
  /** 合并 PR（POST .../pulls/:num/merge；**仅 admin / 仓库 owner**，merge 即更改权限）。 */
  codeRepoPullMerge: (name: string, num: number, opts?: NexhubOpts): Promise<unknown> =>
    request(
      `/api/v1/coderepo/repos/${encodeURIComponent(name)}/pulls/${num}/merge`,
      { method: 'POST', body: {}, headers: nexhubAuthHeaders(opts) },
    ),
  /** 关闭 PR（POST .../pulls/:num/close；仅作者本人或 admin）。 */
  codeRepoPullClose: (name: string, num: number, opts?: NexhubOpts): Promise<unknown> =>
    request(
      `/api/v1/coderepo/repos/${encodeURIComponent(name)}/pulls/${num}/close`,
      { method: 'POST', body: {}, headers: nexhubAuthHeaders(opts) },
    ),

  // —— NexHub 大厅（发现层：本地仓库发布/分享/一键克隆，component nexhub-lobby）——
  // 写端点鉴权（批次 3 链上身份）：nexhub token（Bearer，NexhubOpts 覆盖注入，
  // 服务端反查 pubkey 归因——body 自报 publisher/buyer/hunter/poster 一律忽略）
  // → 系统 admin token 回落 → 401；非 owner 的链上身份操作他人条目 403
  // 「仅项目所有者可操作」。
  /** 获取 nexhub 挑战 nonce（POST /api/v1/nexhub/auth/challenge，公开）。 */
  nexhubAuthChallenge: (pubkey: string): Promise<NexhubAuthChallengeResp> =>
    post<NexhubAuthChallengeResp>('/api/v1/nexhub/auth/challenge', { pubkey }),
  /** 验签换取 nexhub token（POST /api/v1/nexhub/auth/verify，公开；签名 65 字节 r||s||v hex）。 */
  nexhubAuthVerify: (
    pubkey: string,
    nonce: string,
    signature: string,
  ): Promise<NexhubAuthVerifyResp> =>
    post<NexhubAuthVerifyResp>('/api/v1/nexhub/auth/verify', {
      pubkey,
      nonce,
      signature,
    }),
  /**
   * 大厅列表（GET /api/v1/nexhub/lobby[?q=&tag=&sort=]）→ LobbyEntry[]（发布元数据
   * 快照，浏览零开销）。P3 联邦：远程条目带 source_node（非 'local' = 经 os-p2p
   * 从其他 NexOS 节点同步，前端显示 🌐 远程徽章）。
   */
  nexhubLobbyList: (opts?: {
    q?: string;
    tag?: string;
    sort?: 'recent' | 'downloads';
  }): Promise<unknown> => {
    const p = new URLSearchParams();
    if (opts?.q) p.set('q', opts.q);
    if (opts?.tag) p.set('tag', opts.tag);
    if (opts?.sort) p.set('sort', opts.sort);
    const qs = p.toString();
    return get(`/api/v1/nexhub/lobby${qs ? '?' + qs : ''}`);
  },
  /** 大厅条目详情（GET /api/v1/nexhub/lobby/:name）→ 条目 + clone_url_ssh/clone_url_http 双通道地址。 */
  nexhubLobbyDetail: (name: string): Promise<unknown> =>
    get(`/api/v1/nexhub/lobby/${encodeURIComponent(name)}`),
  /**
   * 发布本地仓库到大厅（POST /api/v1/nexhub/lobby/publish，201）；重复发布=刷新快照。
   * **只写本地大厅，不联邦广播**——联邦推送走 nexhubLobbyFederate（两步联邦）。
   * 带 nexhubToken 时 publisher=pubkey（body 自报忽略）、响应含 owner_kind=pubkey +
   * publisher_display；admin 回落时保留 body.publisher（缺省 local）。
   * 重发布权限：owner_kind=pubkey 条目仅同 pubkey 或 admin；他人 403「仅项目所有者可操作」。
   */
  nexhubLobbyPublish: (
    body: {
      repo: string;
      description?: string;
      tags?: string[];
      /** 仅 admin 回落通道生效；链上身份下一律忽略（归因 pubkey）。 */
      publisher?: string;
      /** 价格（最小货币单位；0/省略=免费）。 */
      price_sats?: number;
      /** free / btc / nex / usdc / eth（price_sats>0 时不得为 free）。 */
      currency?: string;
    },
    opts?: NexhubOpts,
  ): Promise<unknown> =>
    request('/api/v1/nexhub/lobby/publish', {
      method: 'POST',
      body,
      headers: nexhubAuthHeaders(opts),
    }),
  /**
   * 下架大厅条目（DELETE /api/v1/nexhub/lobby/:name；仅删条目，本地仓库不动）。
   * 权限：owner 同 pubkey 或 admin；链上身份操作他人/平台托管条目 403「仅项目所有者可操作」。
   */
  nexhubLobbyUnpublish: (name: string, opts?: NexhubOpts): Promise<unknown> =>
    request(`/api/v1/nexhub/lobby/${encodeURIComponent(name)}`, {
      method: 'DELETE',
      headers: nexhubAuthHeaders(opts),
    }),
  /**
   * 推送/重新推送大厅条目到联邦大厅（POST /api/v1/nexhub/lobby/:name/federate）。
   * 两步联邦第二步：条目须已发布在**本地大厅**（不存在直接发布到联邦的路径）；
   * 服务端置 federated=true 并向已连接 peer 广播最新快照，重复调用=重新推送
   * （对端同源刷新）。权限：owner 同 pubkey 或 admin，越权 403。
   */
  nexhubLobbyFederate: (name: string, opts?: NexhubOpts): Promise<unknown> =>
    request(`/api/v1/nexhub/lobby/${encodeURIComponent(name)}/federate`, {
      method: 'POST',
      body: {},
      headers: nexhubAuthHeaders(opts),
    }),
  /**
   * 购买付费条目授权（POST /api/v1/nexhub/lobby/:name/purchase）。
   * buyer = token 身份（body 自报忽略）；免费条目 400；金额/货币校验不过 402。
   */
  nexhubLobbyPurchase: (
    name: string,
    body: {
      /** 支付交易哈希（自证收据）。 */
      txid: string;
      chain?: string;
      amount_sats?: number;
      currency?: string;
    },
    opts?: NexhubOpts,
  ): Promise<unknown> =>
    request(`/api/v1/nexhub/lobby/${encodeURIComponent(name)}/purchase`, {
      method: 'POST',
      body,
      headers: nexhubAuthHeaders(opts),
    }),
  /**
   * 克隆大厅条目到本地（POST /api/v1/nexhub/lobby/:name/clone；需身份——链上
   * token 或 admin；成功 download_count+1）。付费条目需先 purchase 或 owner 豁免，
   * 否则 402 提示先购买。P3 联邦：远程条目（source_node 非 'local'）按原始
   * source_url 从远程节点拉取，响应带 source_node + note。
   */
  nexhubLobbyClone: (name: string, opts?: NexhubOpts): Promise<unknown> =>
    request(`/api/v1/nexhub/lobby/${encodeURIComponent(name)}/clone`, {
      method: 'POST',
      body: {},
      headers: nexhubAuthHeaders(opts),
    }),
  /** 大厅统计（GET /api/v1/nexhub/lobby/stats）→ { published_count, total_downloads, top_tags: [{tag,count}] }。 */
  nexhubLobbyStats: (): Promise<unknown> => get('/api/v1/nexhub/lobby/stats'),

  // —— NexHub 悬赏（bounty，出资求活层；读公开，写需身份：链上 token 或 admin）——
  /** 悬赏列表（GET /api/v1/nexhub/bounty[?status=&q=]）。 */
  nexhubBountyList: (opts?: { status?: string; q?: string }): Promise<unknown> => {
    const p = new URLSearchParams();
    if (opts?.status) p.set('status', opts.status);
    if (opts?.q) p.set('q', opts.q);
    const qs = p.toString();
    return get(`/api/v1/nexhub/bounty${qs ? '?' + qs : ''}`);
  },
  /** 发布悬赏（POST /api/v1/nexhub/bounty；poster=token 身份，reward 必须 >0）。 */
  nexhubBountyCreate: (
    body: {
      title: string;
      description?: string;
      tags?: string[];
      reward_sats: number;
      currency?: string;
      target_url?: string;
      deadline?: string;
    },
    opts?: NexhubOpts,
  ): Promise<unknown> =>
    request('/api/v1/nexhub/bounty', {
      method: 'POST',
      body,
      headers: nexhubAuthHeaders(opts),
    }),
  /** 认领悬赏（POST /api/v1/nexhub/bounty/:id/claim；hunter=token 身份，非 open 409）。 */
  nexhubBountyClaim: (id: string, opts?: NexhubOpts): Promise<unknown> =>
    request(`/api/v1/nexhub/bounty/${encodeURIComponent(id)}/claim`, {
      method: 'POST',
      body: {},
      headers: nexhubAuthHeaders(opts),
    }),
  /** 提交交付物（POST /api/v1/nexhub/bounty/:id/submit；claimed 状态仅认领者本人，越权 403）。 */
  nexhubBountySubmit: (id: string, solutionUrl: string, opts?: NexhubOpts): Promise<unknown> =>
    request(`/api/v1/nexhub/bounty/${encodeURIComponent(id)}/submit`, {
      method: 'POST',
      body: { solution_url: solutionUrl },
      headers: nexhubAuthHeaders(opts),
    }),
  /** 验收支付（POST /api/v1/nexhub/bounty/:id/approve；仅 poster，403「仅悬赏发布者（poster）可操作」）。 */
  nexhubBountyApprove: (id: string, payoutTxid?: string, opts?: NexhubOpts): Promise<unknown> =>
    request(`/api/v1/nexhub/bounty/${encodeURIComponent(id)}/approve`, {
      method: 'POST',
      body: payoutTxid ? { payout_txid: payoutTxid } : {},
      headers: nexhubAuthHeaders(opts),
    }),
  /** 驳回（POST /api/v1/nexhub/bounty/:id/reject；仅 poster，→ open 重开）。 */
  nexhubBountyReject: (id: string, reason?: string, opts?: NexhubOpts): Promise<unknown> =>
    request(`/api/v1/nexhub/bounty/${encodeURIComponent(id)}/reject`, {
      method: 'POST',
      body: reason ? { reason } : {},
      headers: nexhubAuthHeaders(opts),
    }),
  /** 取消（POST /api/v1/nexhub/bounty/:id/cancel；仅 poster，open→cancelled）。 */
  nexhubBountyCancel: (id: string, opts?: NexhubOpts): Promise<unknown> =>
    request(`/api/v1/nexhub/bounty/${encodeURIComponent(id)}/cancel`, {
      method: 'POST',
      body: {},
      headers: nexhubAuthHeaders(opts),
    }),

  // —— API 大厅（推理服务市场 api-market；读公开，写=链上 token 唯一通道无 admin 回落）——
  /**
   * 大厅列表（GET /api/v1/api-market[?q=&sort=&scope=]）→ 挂牌数组（平铺——
   * 联邦化后仍保持数组形态，向后兼容）。
   * - `q`：搜索词（命中 api_name/description/tags，URL 编码）
   * - `sort`：`recent`（默认，最新上架）| `price`（付费单价升序在前、免费垫底）
   * - `scope`（2026-08-31 联邦化）：`all`（默认，本地+联邦平铺）| `local`（仅
   *   本机发布）| `fed`（仅联邦远程条目）
   * - 条目 `source_node`：本地='local'/缺省，远程=发布节点名（「联邦大厅」Tab
   *   过滤依据）；`federated`：发布侧已推送联邦标志；`access_info.api_key`
   *   按视角脱敏（本人/admin 明文，其他 `<前4>***<后4>`）。
   */
  apiMarketList: (
    opts?: { q?: string; sort?: 'recent' | 'price'; scope?: 'all' | 'local' | 'fed' },
  ): Promise<unknown> => {
    const p = new URLSearchParams();
    if (opts?.q) p.set('q', opts.q);
    if (opts?.sort) p.set('sort', opts.sort);
    if (opts?.scope) p.set('scope', opts.scope);
    const qs = p.toString();
    return get(`/api/v1/api-market${qs ? '?' + qs : ''}`);
  },
  /** 条目详情（GET /api/v1/api-market/:id，公开）→ 全字段 + 派生 heartbeat_fresh（≤60s）。 */
  apiMarketDetail: (id: string): Promise<unknown> =>
    get(`/api/v1/api-market/${encodeURIComponent(id)}`),
  /**
   * 挂牌（POST /api/v1/api-market/publish，链上 token；publisher_pubkey=token 反查
   * 不可自报）。新挂 201 / 同名同 pubkey 刷新 200（`refreshed:true`，保留
   * id/created_at/download_count/heartbeat）。server_config 硬件字段自动探测，
   * body 覆盖优先；model_name 探测不到必填（缺 400）。
   */
  apiMarketPublish: (body: ApiMarketPublishBody, opts?: NexhubOpts): Promise<unknown> =>
    request('/api/v1/api-market/publish', {
      method: 'POST',
      body,
      headers: nexhubAuthHeaders(opts),
    }),
  /**
   * 下架（DELETE /api/v1/api-market/:id，链上 token owner；他人 403「仅发布者可下架」，
   * admin token 401——无回落）。物理删行，status 列预留软下线态。
   */
  apiMarketUnpublish: (id: string, opts?: NexhubOpts): Promise<unknown> =>
    request(`/api/v1/api-market/${encodeURIComponent(id)}`, {
      method: 'DELETE',
      headers: nexhubAuthHeaders(opts),
    }),
  /**
   * 心跳自报负载（POST /api/v1/api-market/:id/heartbeat，链上 token owner；他人 403）。
   * 6 键全可选（别名表详见 docs/API_MARKET.md §5.4）→ 更新 heartbeat_at + load
   * （≤60s 视为新鲜）。响应 `{ok, id, heartbeat_at, stale:false, load}`。
   */
  apiMarketHeartbeat: (
    id: string,
    body?: ApiMarketHeartbeatBody,
    opts?: NexhubOpts,
  ): Promise<unknown> =>
    request(`/api/v1/api-market/${encodeURIComponent(id)}/heartbeat`, {
      method: 'POST',
      body: body ?? {},
      headers: nexhubAuthHeaders(opts),
    }),
  /**
   * 负载监控输出（GET /api/v1/api-market/:id/metrics，公开）三态：
   * - 新鲜心跳（≤60s）→ `{reachable:true, stale:false, source:"heartbeat", metrics, ts}`
   * - 无新鲜心跳但有 metrics_url → 服务端代拉（5s 超时，stale:true）
   * - 拉不到/无来源 → `{reachable:false, stale:true, metrics:null, ts:最后心跳|null, error?}`
   * metrics 6 键（load_pct/running/waiting/gpu_cache/tokens_per_sec/latency_ms）
   * 未知为 null 不出现在 JSON——前端按「未知」渲染，不猜。
   */
  apiMarketMetrics: (id: string): Promise<unknown> =>
    get(`/api/v1/api-market/${encodeURIComponent(id)}/metrics`),
  /**
   * 推送/重新推送到联邦大厅（POST /api/v1/api-market/:id/federate，链上 token
   * owner；他人 403「仅发布者可推送联邦」，admin 401 无回落）。两步联邦第二步：
   * publish 只写本地，本端点置 federated=true 并广播 `api_market_lobby` 载荷；
   * 重复调用=重新广播（响应 `first_push:false`）。仅本地条目可推送（联邦远程
   * 副本 403 引导回源节点）。P2P 未启用时广播静默跳过但标志仍置位。
   */
  apiMarketFederate: (id: string, opts?: NexhubOpts): Promise<unknown> =>
    request(`/api/v1/api-market/${encodeURIComponent(id)}/federate`, {
      method: 'POST',
      body: {},
      headers: nexhubAuthHeaders(opts),
    }),


  // —— 远程转发（forwarding：SSH 隧道 spawn 系统 ssh + RDP 纯 Rust TCP 代理）——
  // 契约：GET 免认证；写操作（创建/删除/启停）需 admin Bearer。
  // SSH 密钥认证红线：创建 body 禁止 password 字段（服务端 400），只走 private_key_path。
  /** 列全部 SSH 隧道（GET /api/v1/forwarding/ssh）→ SshTunnel[]。 */
  forwardingSshTunnels: (): Promise<SshTunnel[]> => get<SshTunnel[]>('/api/v1/forwarding/ssh'),
  /** 创建 SSH 隧道（POST /api/v1/forwarding/ssh，需 admin，201）→ SshTunnel。 */
  createForwardingSshTunnel: (body: SshTunnelCreateBody): Promise<SshTunnel> =>
    post<SshTunnel>('/api/v1/forwarding/ssh', body),
  /** 单条 SSH 隧道（GET /api/v1/forwarding/ssh/:id）→ SshTunnel。 */
  getForwardingSshTunnel: (id: string): Promise<SshTunnel> =>
    get<SshTunnel>(`/api/v1/forwarding/ssh/${encodeURIComponent(id)}`),
  /** 删除 SSH 隧道（DELETE /api/v1/forwarding/ssh/:id，需 admin，运行中先停）。 */
  deleteForwardingSshTunnel: (id: string): Promise<unknown> =>
    del(`/api/v1/forwarding/ssh/${encodeURIComponent(id)}`),
  /** 启动 SSH 隧道（POST /api/v1/forwarding/ssh/:id/start，需 admin，spawn ssh -L/-R/-D）。 */
  startForwardingSshTunnel: (id: string): Promise<SshTunnel> =>
    post<SshTunnel>(`/api/v1/forwarding/ssh/${encodeURIComponent(id)}/start`, {}),
  /** 停止 SSH 隧道（POST /api/v1/forwarding/ssh/:id/stop，需 admin，kill 子进程）。 */
  stopForwardingSshTunnel: (id: string): Promise<SshTunnel> =>
    post<SshTunnel>(`/api/v1/forwarding/ssh/${encodeURIComponent(id)}/stop`, {}),

  /** 列全部 RDP 转发（GET /api/v1/forwarding/rdp）→ RdpForward[]。 */
  forwardingRdpForwards: (): Promise<RdpForward[]> => get<RdpForward[]>('/api/v1/forwarding/rdp'),
  /** 创建 RDP 转发（POST /api/v1/forwarding/rdp，需 admin，201）→ RdpForward。 */
  createForwardingRdp: (body: RdpForwardCreateBody): Promise<RdpForward> =>
    post<RdpForward>('/api/v1/forwarding/rdp', body),
  /** 删除 RDP 转发（DELETE /api/v1/forwarding/rdp/:id，需 admin，运行中先停）。 */
  deleteForwardingRdp: (id: string): Promise<unknown> =>
    del(`/api/v1/forwarding/rdp/${encodeURIComponent(id)}`),
  /** 启动 RDP TCP 代理（POST /api/v1/forwarding/rdp/:id/start，需 admin，0.0.0.0:listen→target）。 */
  startForwardingRdp: (id: string): Promise<RdpForward> =>
    post<RdpForward>(`/api/v1/forwarding/rdp/${encodeURIComponent(id)}/start`, {}),
  /** 停止 RDP 代理（POST /api/v1/forwarding/rdp/:id/stop，需 admin，abort accept loop）。 */
  stopForwardingRdp: (id: string): Promise<RdpForward> =>
    post<RdpForward>(`/api/v1/forwarding/rdp/${encodeURIComponent(id)}/stop`, {}),

  /**
   * .rdp 客户端配置文件下载 URL（GET /api/v1/forwarding/rdp/:id/rdp-file?username=）。
   *
   * GET 免认证——可直接 window.open / <a href> 直链下载；username 选填
   * （写入 .rdp 的 username 字段，省略则由客户端提示）。Host 头解析本机地址，
   * 解析失败回退 127.0.0.1。
   */
  forwardingRdpFileUrl: (id: string, username?: string): string =>
    `/api/v1/forwarding/rdp/${encodeURIComponent(id)}/rdp-file${
      username ? `?username=${encodeURIComponent(username)}` : ''
    }`,

  /** 转发统计（GET /api/v1/forwarding/stats）→ 两类总数/运行数 + RDP 累计连接。 */
  forwardingStats: (): Promise<ForwardingStats> => get<ForwardingStats>('/api/v1/forwarding/stats'),

  // —— 更新（update：NexHub tag 更新源 + 通道过滤 + A/B 槽位任务）——
  // 契约：GET 免认证；写操作（切换通道/检查/应用）需 admin Bearer。
  // 开发期 apply 不执行真实镜像下载/写槽——任务推进到 writing 后标记
  // "通道已预留"（note 字段），语义见 docs/UPDATE_APP.md。
  /** 更新总览（GET /api/v1/update/status）→ 当前版本/通道/槽位/上次检查/待应用清单。 */
  updateStatus: (): Promise<UpdateStatusResp> => get<UpdateStatusResp>('/api/v1/update/status'),
  /** 通道目录 + 当前通道（GET /api/v1/update/channels）。 */
  updateChannels: (): Promise<UpdateChannelsResp> =>
    get<UpdateChannelsResp>('/api/v1/update/channels'),
  /** 切换更新通道（POST /api/v1/update/channel，需 admin，JSON 持久化）→ {channel}。 */
  updateSetChannel: (channel: UpdateChannel): Promise<{ channel: UpdateChannel }> =>
    post<{ channel: UpdateChannel }>('/api/v1/update/channel', { channel }),
  /** 检查更新（POST /api/v1/update/check，需 admin；读 NexHub tag → 通道过滤 → semver 比较）。 */
  updateCheck: (): Promise<UpdateCheckResp> => post<UpdateCheckResp>('/api/v1/update/check', {}),
  /** 应用更新（POST /api/v1/update/apply，需 admin，201）→ pending 任务。 */
  updateApply: (version: string): Promise<UpdateTask> =>
    post<UpdateTask>('/api/v1/update/apply', { version }),
  /** 更新任务列表（GET /api/v1/update/tasks，新在前）。 */
  updateTasks: (): Promise<UpdateTask[]> => get<UpdateTask[]>('/api/v1/update/tasks'),
  /** 单个更新任务（GET /api/v1/update/tasks/:id；每次轮询推进一步状态机）。 */
  updateTask: (id: string): Promise<UpdateTask> =>
    get<UpdateTask>(`/api/v1/update/tasks/${encodeURIComponent(id)}`),
  /** 已应用历史（GET /api/v1/update/history；done/reboot_pending 任务，新在前）。 */
  updateHistory: (): Promise<UpdateTask[]> => get<UpdateTask[]>('/api/v1/update/history'),

  // —— 开发者中心（devdocs：仓库 docs/ 唯一事实源的只读服务层）——
  // 契约：全部 GET 免认证（开发期公开读）；文档随 git push 更新（索引缓存 30s）。
  /** 文档索引（GET /api/v1/devdocs/index）→ 分类分组的文档清单 + 根路径/降级说明。 */
  devdocsIndex: (): Promise<DevDocsIndexResp> => get<DevDocsIndexResp>('/api/v1/devdocs/index'),
  /** 单篇 Markdown 原文（GET /api/v1/devdocs/doc/*path；仅 .md，后端防穿越）。 */
  devdocsDoc: (path: string): Promise<DevDocResp> =>
    get<DevDocResp>(
      `/api/v1/devdocs/doc/${path
        .split('/')
        .map(encodeURIComponent)
        .join('/')}`,
    ),

  // —— 「管理」桌面应用（Web 终端，/api/v1/terminal/*，全部 admin）——
  // WS 直连不走本客户端（fetch 无双向流）：
  //   new WebSocket(`${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws/terminal/<id>?token=<admin token>`)
  /** 活跃终端会话列表（GET /api/v1/terminal/sessions，admin）。 */
  terminalSessions: (): Promise<TerminalSession[]> =>
    get<TerminalSession[]>('/api/v1/terminal/sessions'),
  /** 创建终端会话（POST /api/v1/terminal/sessions，admin；本地 shell 或
   *  `ssh -tt`——密码提示经 PTY 透传到 xterm.js，直接在终端里输）。 */
  terminalCreateSession: (body: TerminalCreateBody): Promise<TerminalSession> =>
    post<TerminalSession>('/api/v1/terminal/sessions', body),
  /** 删除终端会话（DELETE /api/v1/terminal/sessions/:id，admin；kill 进程组 + 关 PTY）。 */
  terminalDeleteSession: (id: string): Promise<null> =>
    del<null>(`/api/v1/terminal/sessions/${encodeURIComponent(id)}`),
  /** 节点状态快照（GET /api/v1/terminal/node-snapshot，admin）——管理页顶部
   *  状态条：版本/在线时长/P2P 连接数/磁盘/内存一次性聚合。 */
  terminalNodeSnapshot: (): Promise<TerminalNodeSnapshot> =>
    get<TerminalNodeSnapshot>('/api/v1/terminal/node-snapshot'),

  // —— 统一打赏（tips：链上身份账本，component=tips，docs/TIPS.md）——
  // 打赏 = 一条真实账本记录（from 链上身份 pubkey → to 目标所有者 pubkey，
  // 服务端反查防自报伪造）；amount 站内积分，txid 自报链上凭证（不验真）。
  // target_kind ∈ im_message | lobby_entry | node；ref 格式见 docs/TIPS.md 映射表。
  /** 打赏入账（POST /api/v1/tips → 202）。身份：链上 token（IM/NexHub 皆可，
   *  opts.token 注入 Bearer）优先，无 token 回落网关 Principal（测试期默认
   *  admin）。to_pubkey 由服务端按 target 反查，目标不存在/所有者非链上身份
   *  → 400。 */
  tipCreate: (
    body: TipCreateBody,
    opts?: TipOpts,
  ): Promise<TipCreateResp> =>
    request<TipCreateResp>('/api/v1/tips', {
      method: 'POST',
      body,
      headers: tipsAuthHeaders(opts),
    }),
  /** 目标聚合（GET /api/v1/tips/target/:kind/:ref，公开读）→ 累计金额/次数 +
   *  最近 20 条脱敏（from 前缀/金额/留言/时间）。四大厅条目卡片并行拉取。 */
  tipsTarget: (kind: TipTargetKind, ref: string): Promise<TipsTargetResp> =>
    get<TipsTargetResp>(
      `/api/v1/tips/target/${encodeURIComponent(kind)}/${encodeURIComponent(ref)}`,
    ),
  /** 我的打赏聚合（GET /api/v1/tips/me）→ 按当前身份（链上 token/Principal）
   *  的收到/给出累计与最近记录。 */
  tipsMe: (opts?: TipOpts): Promise<TipsMeResp> =>
    request<TipsMeResp>('/api/v1/tips/me', { headers: tipsAuthHeaders(opts) }),
};

/** 容量工具（同 os-core::Capacity::used_ratio，避免循环依赖 types 内的副本）。 */
export function capacityRatio(c: Capacity | undefined | null): number {
  if (!c || !c.total_bytes) return 0;
  return Math.max(0, Math.min(1, c.used_bytes / c.total_bytes));
}

// —— 统一打赏（tips）类型与鉴权头（docs/TIPS.md 契约）——

/** 打赏目标类型（与后端 tips.rs KINDS 一致）。 */
export type TipTargetKind = 'im_message' | 'lobby_entry' | 'node';

/** POST /api/v1/tips 请求体。 */
export interface TipCreateBody {
  target_kind: TipTargetKind;
  /** 目标引用：im 消息 id / `nexhub:<repo>`|`model:<id>`|`apimarket:<id>` / NodeID。 */
  target_ref: string;
  /** 站内积分（正整数；服务端不虚构链上转账）。 */
  amount: number;
  /** 留言（≤500 字符，可选）。 */
  message?: string;
  /** 用户自报链上转账凭证（≤128 字符，可选；服务端不验真——已知限制）。 */
  txid?: string;
}

/** tips 调用可选项：注入链上 token（IM 或 NexHub 签发皆可，服务端两桶依次验）。 */
export interface TipOpts {
  token?: string;
}

/** POST /api/v1/tips 响应（202；from/to 脱敏前缀）。 */
export interface TipCreateResp {
  ok: boolean;
  id: number;
  from: string;
  to: string;
  target_kind: TipTargetKind;
  target_ref: string;
  amount: number;
  created_at: number;
}

/** 脱敏的最近打赏条目（公开聚合里只见 from 前缀/金额/留言/时间）。 */
export interface TipRecentEntry {
  from: string;
  to: string;
  target_kind: TipTargetKind;
  target_ref: string;
  amount: number;
  message?: string | null;
  txid?: string | null;
  created_at: number;
}

/** GET /api/v1/tips/target/:kind/:ref 响应。 */
export interface TipsTargetResp {
  target_kind: TipTargetKind;
  target_ref: string;
  total: number;
  count: number;
  recent: TipRecentEntry[];
}

/** GET /api/v1/tips/me 响应（按身份的收到/给出聚合）。 */
export interface TipsMeResp {
  identity: string;
  received: { total: number; count: number };
  given: { total: number; count: number };
  recent_received: TipRecentEntry[];
  recent_given: TipRecentEntry[];
}

/** tips 链上 token → Authorization 头（无 token 时 undefined——回落网关
 * Principal，测试期默认 admin；与 nexhubAuthHeaders 同构）。 */
function tipsAuthHeaders(o?: TipOpts): Record<string, string> | undefined {
  return o?.token ? { Authorization: `Bearer ${o.token}` } : undefined;
}

// =============================================================================
// 直播（/api/v1/live/* + WS /ws/live/*）已随流媒体中心剥离为独立应用包
// apps/streaming（NexHub nexos-app-streaming）：LiveRoom 等类型与 liveCreateRoom
// 等函数、WS URL 构造迁入其 src/api.ts。live 端点后端常开（直播联邦能力 UI 随
// 应用包走，引擎端点不门控）。
// =============================================================================

// —— 开发者中心 AI 翻译（devdocs ?lang= 管线，docs/DEVDOCS_DEV_CENTER.md）——

/** GET /api/v1/devdocs/translate/tasks/:id 响应（翻译任务视图，轮询用）。 */
export interface DevDocsTranslateTask {
  id: string;
  /** 目标语言目录名（`en` / `zh-TW`） */
  lang: string;
  /** 文档相对路径 */
  path: string;
  /** `running` | `done` | `error` */
  status: string;
  /** 分块总数（分块是纯函数，任务创建时即已知） */
  chunks_total: number;
  /** 已完成块数（进度展示） */
  chunks_done: number;
  /** 环形日志（服务端截 200 行） */
  log: string[];
  started_at: number;
  finished_at: number | null;
  /** 失败原因（status=error 时；503 降级文案同源） */
  error: string | null;
}

/** GET doc?lang=<目标> 的两种形态：200 = 译文（缓存命中）；202 = 翻译任务进行中。 */
export type DevDocsLangDoc =
  | { kind: 'doc'; doc: DevDocResp }
  | { kind: 'task'; task: DevDocsTranslateTask };

/**
 * 取（可能翻译的）文档（GET /api/v1/devdocs/doc/*path?lang=<en|zh-TW>）：
 * - 200 → 译文（缓存命中；响应头 X-Translation: cached）；
 * - 202 → 翻译任务进行中（用 devdocsTranslateTask 轮询，done 后重取）；
 * - 503 → 抛 ApiError（message 含服务端降级文案，如「本节点无可用本地模型…」）。
 *
 * `retry=1`：上一任务失败后强制重试（清除失败态重新翻译）。
 * 注：不能用通用 `get()`（它丢弃 status，无法区分 200/202），故单独 fetch。
 */
export async function devdocsDocLang(
  path: string,
  lang: string,
  retry = false,
): Promise<DevDocsLangDoc> {
  const encoded = path
    .split('/')
    .map(encodeURIComponent)
    .join('/');
  const qs = new URLSearchParams({ lang });
  if (retry) qs.set('retry', '1');
  const headers: Record<string, string> = { Accept: 'application/json' };
  const token = getApiToken();
  if (typeof token === 'string' && token.trim()) {
    headers['Authorization'] = `Bearer ${token}`;
  }
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), TIMEOUT_MS);
  try {
    const resp = await fetch(
      `${BASE_URL}/api/v1/devdocs/doc/${encoded}?${qs.toString()}`,
      { headers, signal: controller.signal },
    );
    const body = (await resp.json().catch(() => null)) as
      | (DevDocResp & Partial<DevDocsTranslateTask>)
      | null;
    if (!resp.ok) {
      const detail =
        body && typeof body.error === 'string' ? body.error : `${resp.status} ${resp.statusText}`;
      throw new ApiError(`${resp.status} ${resp.statusText} — ${detail}`, {
        status: resp.status,
        path: encoded,
      });
    }
    if (resp.status === 202 && body && body.id && body.status) {
      return { kind: 'task', task: body as unknown as DevDocsTranslateTask };
    }
    if (!body || typeof body.markdown !== 'string') {
      throw new ApiError('文档响应格式异常（缺少 markdown）', { path: encoded });
    }
    return { kind: 'doc', doc: body };
  } finally {
    clearTimeout(timer);
  }
}

/** 翻译任务视图（GET /api/v1/devdocs/translate/tasks/:id；未知 id 抛 404）。 */
export function devdocsTranslateTask(id: string): Promise<DevDocsTranslateTask> {
  return get<DevDocsTranslateTask>(
    `/api/v1/devdocs/translate/tasks/${encodeURIComponent(id)}`,
  );
}

// =============================================================================
// —— 应用包运行时（/api/v1/apps/*，docs/APPS.md）——
//
// 应用包（原内置应用剥离产物，apps/<id>，NexHub 仓库）：
//   - GET    /api/v1/apps           已安装应用包列表
//   - GET    /api/v1/apps/catalog   可安装目录（NexHub 应用包，含安装状态）
//   - POST   /api/v1/apps/install   安装 {repo}（git clone + 落盘）
//   - DELETE /api/v1/apps/:id       卸载（含静态资源 /apps-assets/:id）
// 静态资源：GET /apps-assets/:id/<path>（entry.js 从这加载，主前端 dynamic import）。
// 前端运行时注册流见 src/appRuntime.ts（register(ctx) 协议冻结，docs/APPS.md）。
// =============================================================================

/** 已安装应用包（GET /api/v1/apps 的 apps 元素；字段见 docs/APPS.md）。 */
export interface InstalledApp {
  /** 应用 id（= 目录名 = /apps-assets/:id 段）。 */
  id: string;
  /** 显示名。 */
  name: string;
  /** 版本（manifest.version）。 */
  version: string;
  /** 分类（manifest.category）。 */
  category: string;
  /** 图标名（manifest.icon）。 */
  icon: string;
  /** 一句话简介。 */
  description: string;
  /** entry 相对路径（相对 /apps-assets/:id/，如 web/entry.js）。 */
  entry: string;
  /** 安装目录（服务端信息，前端仅展示）。 */
  dir: string;
  /** 安装时间（ISO）。 */
  installed_at: string;
}

/** 目录应用包（GET /api/v1/apps/catalog 的 apps 元素）。 */
export interface CatalogApp {
  /** NexHub 仓库名（安装入参）。 */
  repo: string;
  /** 应用 id。 */
  id: string;
  name: string;
  version: string;
  category: string;
  icon: string;
  description: string;
  /** 是否已安装。 */
  installed: boolean;
  /** 已装版本（installed 时可能带）。 */
  installed_version?: string;
  /** 引擎名（manifest.engine；film/streaming/qrtransfer 等引擎门控联动）。 */
  engine?: string;
  /** manifest 校验失败原因（非空 = 该仓库 manifest.json 缺失或校验不过）。 */
  error?: string;
}

/** 已安装列表（GET /api/v1/apps）。 */
export interface AppsListResp {
  apps: InstalledApp[];
}

/** 目录列表（GET /api/v1/apps/catalog）。 */
export interface AppsCatalogResp {
  apps: CatalogApp[];
}

/** 安装结果（POST /api/v1/apps/install）：同步结果或安装任务（后端实现决定，
 *  前端宽松容错——以 /api/v1/apps 的最终状态为准）。 */
export interface AppsInstallResp {
  /** 兼容同步完成的直接返回（{app: InstalledApp} / InstalledApp）。 */
  app?: InstalledApp;
  /** 兼容任务态返回（轮询 /api/v1/apps 观察 id 出现）。 */
  task?: { id?: string; status?: string };
  status?: string;
  [k: string]: unknown;
}

/** 已安装应用包列表（GET /api/v1/apps）。 */
export function appsList(): Promise<AppsListResp> {
  return get<AppsListResp>('/api/v1/apps');
}

/** 应用包目录（GET /api/v1/apps/catalog）。 */
export function appsCatalog(): Promise<AppsCatalogResp> {
  return get<AppsCatalogResp>('/api/v1/apps/catalog');
}

/** 安装应用包（POST /api/v1/apps/install {repo}）。 */
export function appsInstall(repo: string): Promise<AppsInstallResp> {
  return post<AppsInstallResp>('/api/v1/apps/install', { repo });
}

/** 卸载应用包（DELETE /api/v1/apps/:id）。 */
export function appsUninstall(id: string): Promise<null> {
  return del<null>(`/api/v1/apps/${encodeURIComponent(id)}`);
}

// =============================================================================
// —— NexHub 内置 CI（/api/v1/coderepo/*ci，v0.1.33，nexhub_ci 组件）——
//
//   - POST   /api/v1/coderepo/repos/:name/ci        手动触发（admin，202）
//   - GET    /api/v1/coderepo/repos/:name/ci        runs 列表（最新 20）
//   - GET    /api/v1/coderepo/repos/:name/ci/:run   详情 + 环形日志（500 行）
//   - DELETE /api/v1/coderepo/repos/:name/ci/:run   删记录（admin，终态可删）
//   - GET    /api/v1/coderepo/ci/latest             各仓最新 run 摘要（徽章）
// =============================================================================

/** CI run（nexhub_ci）。log 仅详情端点返回。 */
export interface CiRun {
  id: string;
  repo_name: string;
  /** push（push 自动触发）| manual（手动）。 */
  trigger: 'push' | 'manual' | string;
  /** queued | running | passed | failed | skipped。 */
  status: 'queued' | 'running' | 'passed' | 'failed' | 'skipped' | string;
  /** 流水线命令描述（如 `npm ci && npm run build`；skipped 为空）。 */
  pipeline?: string | null;
  exit_code?: number | null;
  created_at?: string;
  started_at?: string | null;
  finished_at?: string | null;
  /** 运行耗时毫秒。 */
  duration_ms?: number | null;
  log?: string;
}

/** 手动触发响应（202）。 */
export interface CiTriggerResp {
  ok: boolean;
  run?: CiRun;
}

/** runs 列表响应（最新 20，不含 log）。 */
export interface CiRunsResp {
  repo: string;
  runs: CiRun[];
}

/** run 详情响应（含 log）。 */
export interface CiRunResp {
  run: CiRun;
}

/** 各仓最新 run 摘要聚合响应（仓库卡徽章一次拉全）。 */
export interface CiLatestResp {
  latest: CiRun[];
}

/** 手动触发 CI（POST /api/v1/coderepo/repos/:name/ci，需 admin）。 */
export function codeRepoCiTrigger(name: string): Promise<CiTriggerResp> {
  return post<CiTriggerResp>(`/api/v1/coderepo/repos/${encodeURIComponent(name)}/ci`, {});
}

/** 该仓 CI runs（GET /api/v1/coderepo/repos/:name/ci，最新 20）。 */
export function codeRepoCiRuns(name: string): Promise<CiRunsResp> {
  return get<CiRunsResp>(`/api/v1/coderepo/repos/${encodeURIComponent(name)}/ci`);
}

/** run 详情 + 日志全文（GET /api/v1/coderepo/repos/:name/ci/:runId）。 */
export function codeRepoCiRun(name: string, runId: string): Promise<CiRunResp> {
  return get<CiRunResp>(
    `/api/v1/coderepo/repos/${encodeURIComponent(name)}/ci/${encodeURIComponent(runId)}`,
  );
}

/** 删除 run 记录（DELETE，需 admin；queued/running 409）。 */
export function codeRepoCiDeleteRun(name: string, runId: string): Promise<{ ok: boolean; id: string }> {
  return del<{ ok: boolean; id: string }>(
    `/api/v1/coderepo/repos/${encodeURIComponent(name)}/ci/${encodeURIComponent(runId)}`,
  );
}

/** 各仓最新 run 摘要聚合（GET /api/v1/coderepo/ci/latest）。 */
export function codeRepoCiLatest(): Promise<CiLatestResp> {
  return get<CiLatestResp>('/api/v1/coderepo/ci/latest');
}
