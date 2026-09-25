<script setup lang="ts">
// =============================================================================
// Chat.vue —— IM 聊天界面（类微信/Telegram Web 双栏布局）
//
// 布局（2026-08-23 会话列表重构：大厅类会话=左侧列表独立会话项，不再用 Tab）：
//   ┌──────────────────┬─────────────────────────────┐
//   │ 身份卡            │  顶栏（标题 · 搜索 · ⚙️设置最右）│
//   │ 🏨 我的大厅       │  消息区 + 输入框              │
//   │ 🌐 联邦大厅(可写) │  [消息气泡 + 头像]            │
//   │ ── 节点大厅 ──    │                              │
//   │ 🏨 node-x 的大厅  │  ┌──────────┐ ┌──────┐       │
//   │ ── 对话 ──        │  │输入消息...│ │发送  │       │
//   │ # 群组1 [3]      │  └──────────┘ └──────┘       │
//   │ ── 节点 ──        │                              │
//   │ 🟢 0x**…a1b2 [87] │  ← P2P 元数据自动发现（仅 Active，30s 轮询，点击进对方大厅）
//   └──────────────────┴─────────────────────────────┘
//
// 会话类型（左侧列表统一模型，activeView 四态）：
//   my-lobby     「我的大厅」＝本节点的房间（conversation_id=lobby）。本地用户 +
//                远程用户（经节点发现直接进入）的发言，消息**只留本节点**（不再
//                自动联邦广播）；发言 POST /im/lobby/messages。
//   fed-lobby    「联邦大厅」＝跨节点共享频道（conversation_id=fed-lobby），与
//                我的大厅**完全隔离**的独立**可写**会话（2026-08-23 用户纠正：
//                非只读聚合流）。数据=GET /im/fed-lobby/messages（独立消息列表）；
//                发言 POST /im/fed-lobby/messages（服务端本地落库 + P2P 广播全部
//                已连接节点）；其他节点用户的发言经 WS im_fed_lobby_message 帧到达
//                （sender_id=fed:<node>:<pubkey>，带 🌐 来源徽章）。
//   remote-lobby 「🏨 <节点名> 的大厅」＝接入的远程节点大厅独立会话项（节点发现页
//                「进入 IM」/chat?node=<id>&name=<名> 创建，localStorage 持久化）。
//                消息 GET /im/lobby/remote/:node_id（只读镜像，激活期 10s 轮询）；
//                发言 POST /im/lobby/remote/:id/messages（经 P2P 送达对方大厅，文本）。
//   group        私聊/群组（原有行为不变：消息缓存 + after_id 增量 + 未读徽章）。
//
// API（os-api /api/v1/im/*，批次 2 起全部用户面端点需 IM token）：
//   POST /api/v1/im/auth/challenge {pubkey}              → {nonce}（60s 单次有效，公开）
//   POST /api/v1/im/auth/verify {pubkey,nonce,signature} → {token}(24h)（公开）
//   GET  /api/v1/im/groups                              → 群组/对话列表（IM token）
//   POST /api/v1/im/groups                              → 创建群组 { name, members? }（IM token）
//   GET  /api/v1/im/conversations/:id/messages          → 消息列表（IM token）
//   POST /api/v1/im/conversations/:id/messages          → 发送消息 { content }（IM token）
//   GET  /api/v1/im/messages?conversation_id=&after_id=&limit=
//                                                       → 离线补拉：严格晚于 after_id
//                                                         的消息升序（IM token；群组/大厅
//                                                         非成员 403；limit 1..=200 默认 50）
//   GET  /api/v1/p2p/node-meta                          → 节点元数据注册表（公开；
//                                                       侧栏「节点」分区数据源，仅取
//                                                       Active 条目，30s 轮询自动发现）
//   POST /api/v1/im/messages/:id/read                   → 标记已读（IM token；user 服务端反查）
//   GET  /api/v1/im/conversations/:id/unread            → 未读数（IM token；user 服务端反查）
//   GET  /api/v1/im/search?q=&conversation_id=&limit=  → 搜索消息（IM token；缺省搜
//                                                       大厅，指定搜该会话，倒序 limit
//                                                       1..=200 默认 50；空 q 400）
//   GET  /api/v1/im/lobby                               → 大厅信息（IM token；Bearer 即心跳，自动加入）
//   GET  /api/v1/im/lobby/messages[?after_id=]          → 大厅最近 50 条 / 增量补拉（IM token）
//   POST /api/v1/im/lobby/messages                      → 发大厅消息 { content }（IM token；只留本节点）
//   GET  /api/v1/im/lobby/members                       → 大厅成员（IM token）
//   GET  /api/v1/im/fed-lobby                           → 联邦大厅信息 + 心跳加入（IM token）
//   GET  /api/v1/im/fed-lobby/messages[?after_id=]      → 联邦大厅最近 50 条 / 增量（IM token）
//   POST /api/v1/im/fed-lobby/messages                  → 联邦大厅发言 {content, sender_kind?}
//                                                       （IM token；本地落库 + P2P 广播全部节点）
//   GET  /api/v1/im/federation                          → 联邦接收开关状态（IM token）
//   POST /api/v1/im/federation                          → 切换联邦接收开关 {enabled}
//                                                       （IM 或 admin token；关=暂停接收远程
//                                                        大厅消息，本地消息与发送不受影响）
//   GET  /api/v1/im/lobby/access                        → 大厅开放开关（IM 或 admin token）
//   POST /api/v1/im/lobby/access                        → 切换大厅开放开关 {lobby_public}
//   GET  /api/v1/im/lobby/remote/:node_id               → 对方大厅只读镜像（IM token，
//                                                       经加密 P2P 通道，≤20 条脱敏消息）
//   POST /api/v1/im/lobby/remote/:node_id/messages      → 向对方大厅远程发言（IM token）
//   GET  /api/v1/im/dm/access                           → 直通消息开放开关（IM 或 admin token）
//   POST /api/v1/im/dm/access                           → 切换直通消息开关 {dm_open}（关=
//                                                        他人私信被拒；自己发出不受影响）
//   POST /api/v1/im/dm                                   → 发起点对点直通消息 {to_pubkey,
//                                                       content, to_node?}（IM token；本地
//                                                       投递或经 P2P 定向到对方节点；只有
//                                                       双方可见，确定性 dm-* 会话）
//   GET  /api/v1/im/conversations                       → 对话列表（IM token；dm-* 按成员过滤，
//                                                       members=双方 pubkey）
//   POST /api/v1/im/files                               → 上传附件 {filename, content_base64}
//                                                       （base64-JSON ≤64MiB，IM token）
//   GET  /api/v1/im/files/:file_id?token=               → 附件直链下载（IM token query 形态）
//   WebSocket /ws?user=<pubkey>&token=<IM token>        → 实时推送（无 token 握手 401）
//
// 行为（批次 2 区块链认证，docs/IM_BLOCKCHAIN_AUTH_DESIGN.md）：
//   - 身份 = secp256k1 压缩公钥（0x+66hex），私钥存 localStorage（useImIdentity）
//   - 进入页面自动 ensureAuthenticated（challenge→sign→verify）；失败给重试按钮
//   - 401 自动清 token 重认证一次再重试（withIm 包装全部 IM 调用）
//   - 请求体/查询里的 sender/user 服务端一律忽略（从 token 反查 pubkey）
//   - 自己发的消息（sender_id === pubkey）右对齐橙色气泡，否则左对齐
//   - 选中群组后拉一次消息，并经 WebSocket 实时接收新消息（不再 3s 轮询）
//   - WS 断线重连成功后按本地最后一条消息 id 调补拉端点（/api/v1/im/messages、
//     /lobby/messages?after_id= 与 /fed-lobby/messages?after_id=）把错过的大厅/
//     联邦大厅/会话消息追加进列表（按 id 去重），
//     其它会话的缺口转为未读徽章；切换回曾看过的会话也走 after_id 增量而非全量
//   - 切换对话时标记全部消息已读，对话列表显示未读徽章
//   - 「我的大厅」「联邦大厅」是两个互相隔离的独立会话（2026-08-23 用户纠正）：
//     我的大厅数据=lobbyMessages（本节点房间，消息只留本节点）；联邦大厅数据=
//     fedLobbyMessages（fed-lobby 会话，所有节点的发言）。两路 WS 帧
//     （im_lobby_message / im_fed_lobby_message）与补拉端点各自独立，未在看的
//     那个会话累计未读徽章
//   - 系统消息居中灰色；回复消息显示引用摘要；消息含头像/时间
//
// 行为（批次 3 Agent 可见性 + @助手 + 文档传输，docs/IM_AGENTS_AND_FILES.md）：
//   - sender_kind==="agent" 的消息：气泡名旁 🤖徽章 + 紫描边微区分
//     （"（AI 生成）"后缀由后端带出；字段仅展示语义，不作权限依据）
//   - 正文里的 @提及（mentions 数组命中段）渲染为 accent 色
//   - composer 输入 `@词`（@ 后至光标、无空格/标点）→ 弹成员选择浮层：
//     大厅=在线成员、会话=已知发送者/成员去重，NexOS助手恒置顶；每项
//     identicon 头像 + 展示名（0x 短显）+ agent 徽章；↑↓ 选择，Enter/Tab/
//     点击补全为 `@完整名 `（尾空格），Esc/点外关闭；浮层贴输入框上方
//     （随 composer-row 定宽，窄屏不外溢）。已知限制：外部 agent 是否响应
//     @ 由其自身实现决定（WS mentions 自匹配，docs/IM_AGENTS_AND_FILES.md
//     §6.4）——UI 只保证名字精确补全使服务端 mentions 解析命中
//   - 📎 选文件（前端预检 ≤64MiB）→ 立即 POST /im/files（busy 态）→ 待发
//     附件条（文件名+大小+✕移除）；发送时 body 带 attachment:{file_id}，
//     附件随消息清空暂存；会话视图支持纯附件消息（大厅空白正文 400）
//   - 消息气泡渲染 attachment：📄文件名+大小，点击「下载」fetch 信封直链
//     （imFileUrl，token 已含在 url）→ 解码 content_base64 → Blob 另存；
//     直链对 agent 等 API 消费方保持原样（直接拿 base64 信封自行解码）
//   - 轮询/WS 逻辑不变：sender_kind/mentions/attachment 新字段经既有
//     REST/WS 通道自动透传渲染
// =============================================================================
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { useRoute } from 'vue-router';
import { ApiError, base64ToBytes, endpoints } from '@/api/client';
import type {
  ImDmAccessStatus,
  ImFileUploadResp,
  ImLobbyAccessStatus,
  ImLobbyViewMessage,
  ImMessageAttachment,
  ImMessageExt,
  ImSendExtras,
  P2pNodeMetaEntry,
} from '@/api/client';
import type {
  ImGroup,
  ImLobbyInfo,
  ImLobbyMember,
  } from '@/api/types';
// 「蓝牙 mesh 中继」Tab 已迁入「网络管理」(/network) 的「BLE Mesh 中继」Tab
import { identiconSvg, shortIdentity } from '@/composables/useIdenticon';
import { useImIdentity } from '@/composables/useImIdentity';
import { useToast } from '@/composables/useToast';
import { copyText } from '@/utils/clipboard';
// 统一打赏按钮（docs/TIPS.md：消息级打赏，target_kind=im_message，ref=消息 id）
import TipButton from '@/components/TipButton.vue';

// =============================================================================
// 区块链身份 + 认证（单例 composable：私钥 localStorage，token 内存+localStorage）
// =============================================================================
const toast = useToast(); // 轻提示（IM 设置开关切换成功/失败等）
const {
  hasIdentity,
  pubkey,
  displayName: imDisplayName,
  authenticating: imAuthenticating,
  generateIdentity,
  importIdentity,
  clearIdentity,
  ensureAuthenticated,
  forceReauth,
} = useImIdentity();

/** 认证阶段：no-identity 待初始化 / authing 认证中 / ok 就绪 / error 失败可重试。 */
const authPhase = ref<'no-identity' | 'authing' | 'ok' | 'error'>('no-identity');
const authError = ref('');

/**
 * 统一 IM 调用包装：自动确保认证并带 IM token（覆盖全局 admin token）；
 * 命中 401（token 过期/被顶掉）→ 清 token 重认证一次 → 重试。
 */
async function withIm<T>(fn: (imToken: string) => Promise<T>): Promise<T> {
  const attempt = (t: string) => fn(t);
  let session = await ensureAuthenticated();
  try {
    return await attempt(session.token);
  } catch (e) {
    if (e instanceof ApiError && e.status === 401) {
      forceReauth();
      session = await ensureAuthenticated();
      return await attempt(session.token);
    }
    throw e;
  }
}

/**
 * 打赏（TipButton）链上 token 获取器：已初始化身份 → IM token（服务端反查
 * from pubkey）；未初始化/认证失败 → undefined（回落网关 Principal，测试期
 * 默认 admin 归因）。见 docs/TIPS.md。
 */
async function tipTokenGetter(): Promise<string | undefined> {
  if (!hasIdentity.value) return undefined;
  try {
    return (await ensureAuthenticated()).token;
  } catch {
    return undefined;
  }
}

/** 认证成功后启动聊天数据流（进入页面 / 重试 / 换身份后调用）。 */
function startChat(): void {
  void loadLobby(); // 我的大厅数据（本节点房间）
  void loadFedLobby(); // 联邦大厅数据（跨节点共享频道，独立会话）
  void loadFedReceiveStatus(); // 联邦接收开关状态（暂停横幅 + IM 设置面板共用）
  void loadGroups();
  void loadDms(); // 直通消息会话（dm-* 私信，独立于大厅的双方可见通道）
  startImNodesPolling(); // 侧栏「节点」分区：P2P 元数据自动发现（30s 轮询）
  connectWs();
  startLobbyHeartbeat();
  maybeLoadActiveRemote(); // ?node= 直达远程大厅会话：认证就绪后补拉镜像
}

/** 认证成功后停止全部数据流（换身份 / 卸载时调用）。 */
function teardownChat(): void {
  stopLobbyHeartbeat();
  stopRemoteLobbyPolling();
  stopImNodesPolling();
  wsClosedByUs = true;
  if (wsReconnectTimer !== null) {
    clearTimeout(wsReconnectTimer);
    wsReconnectTimer = null;
  }
  ws?.close();
  ws = null;
  wsHadDrop = false;
  groups.value = [];
  currentGroup.value = null;
  messages.value = [];
  messagesCache.clear();
  imNodes.value = [];
  unreadMap.value = {};
  lobbyInfo.value = null;
  lobbyMessages.value = [];
  lobbyMembers.value = [];
  lobbyError.value = '';
  fedLobbyInfo.value = null;
  fedLobbyMessages.value = [];
  fedLobbyError.value = '';
  myLobbyUnread.value = 0;
  fedLobbyUnread.value = 0;
  fedReceiveEnabled.value = null;
  fedError.value = '';
  showImSettings.value = false; // 切身份时收起 IM 设置面板
  lobbyAccess.value = null;
  lobbyAccessError.value = '';
  dmAccess.value = null; // 直通消息开关与会话随身份重置
  dmAccessError.value = '';
  dmConversations.value = [];
  dmPendingPeers.value = {};
  activeView.value = 'my-lobby';
  activeRemoteNodeId.value = '';
  remoteStates.value = {}; // 远程会话项（remoteLobbies）随 localStorage 保留
}

/** 进入页面 / 点击重试：确保认证（无私钥则停在初始化卡）→ 启动聊天。 */
async function bootstrapAuth(): Promise<void> {
  if (!hasIdentity.value) {
    authPhase.value = 'no-identity';
    return;
  }
  authPhase.value = 'authing';
  authError.value = '';
  try {
    await ensureAuthenticated();
    authPhase.value = 'ok';
    startChat();
  } catch (e) {
    authPhase.value = 'error';
    authError.value = e instanceof Error ? e.message : String(e);
  }
}

// =============================================================================
// 数据：群组 / 消息 / 节点 / 未读 / 搜索
// =============================================================================
const groups = ref<ImGroup[]>([]);
const groupsLoading = ref(false);
const groupsError = ref('');

const currentGroup = ref<ImGroup | null>(null);
const messages = ref<ImMessageExt[]>([]);
const messagesLoading = ref(false);
const messagesError = ref('');

/**
 * 各会话已加载消息的本地缓存（conversation_id → 升序列表，非响应式）。
 * 用途：① WS 断线重连补拉时提供"本地最后一条消息 id"；② 切换回曾看过的
 * 会话时先回填缓存再 after_id 增量补拉，避免全量重拉与白屏闪烁。
 * 与 messages.value 同步维护（全量加载 / 增量补拉 / WS 推送 / 发送成功）。
 */
const messagesCache = new Map<string, ImMessageExt[]>();

/** 把 incoming（服务端保证升序）按 id 去重追加到 list 末尾，返回新数组。 */
function appendDedup(list: ImMessageExt[], incoming: ImMessageExt[]): ImMessageExt[] {
  if (!incoming.length) return list;
  const seen = new Set(list.map((m) => m.id));
  const add = incoming.filter((m) => !seen.has(m.id));
  return add.length ? [...list, ...add] : list;
}

/** 同步某会话的缓存（当前会话直接写 messages.value 的内容）。 */
function syncCache(cid: string, list: ImMessageExt[]): void {
  messagesCache.set(cid, list);
}

// —— 侧栏「节点」分区：P2P 元数据自动发现（2026-08-23 移除「添加节点」控制台）——
// 数据源 GET /api/v1/p2p/node-meta（os-p2p meta 注册表，按健康分降序），
// 30s 轮询自动刷新；只展示 Active 节点，点击即发起对方大厅会话。
/** 节点元数据注册表快照（含 Inactive；展示层过滤 Active）。 */
const imNodes = ref<P2pNodeMetaEntry[]>([]);
/** 节点元数据轮询间隔（meta 注册表由 P2P 心跳维护，30s 轻量刷新足矣）。 */
const IM_NODES_POLL_MS = 30_000;
let imNodesTimer: ReturnType<typeof setInterval> | null = null;

/** 只展示 Active 节点（Inactive 死节点不进 IM 列表，避免对不可达节点发起会话）。 */
const activeImNodes = computed(() => imNodes.value.filter((n) => 'active' in n.state));

/** 拉取节点元数据（失败静默降级：列表空，不打断聊天、不弹错误）。 */
async function loadImNodes(): Promise<void> {
  try {
    const v = await endpoints.p2pNodeMeta();
    imNodes.value = Array.isArray(v) ? v : [];
  } catch {
    imNodes.value = [];
  }
}

/** 启动节点元数据轮询（startChat 调用：立即拉一次 + 30s 周期）。 */
function startImNodesPolling(): void {
  stopImNodesPolling();
  void loadImNodes();
  imNodesTimer = setInterval(() => void loadImNodes(), IM_NODES_POLL_MS);
}

/** 停止节点元数据轮询（teardownChat 调用）。 */
function stopImNodesPolling(): void {
  if (imNodesTimer !== null) {
    clearInterval(imNodesTimer);
    imNodesTimer = null;
  }
}

/** 点击活跃节点 → 发起对方大厅会话（与节点发现页 openIm 的 /chat?node=<id>
 *  跳转等效：本页内直接 upsert + select，避免重复压历史栈）。 */
function openNodeLobby(n: P2pNodeMetaEntry): void {
  upsertRemoteLobby(n.id, shortIdentity(n.id));
  selectRemoteLobby(n.id);
}

/** last_seen（unix 秒）→ 相对时间（"刚刚 / N 分钟前 / N 小时前 / N 天前"）。 */
function relTime(ts: number): string {
  if (!ts) return '从未';
  const sec = Math.max(0, Math.floor(Date.now() / 1000 - ts));
  if (sec < 60) return '刚刚';
  if (sec < 3600) return `${Math.floor(sec / 60)} 分钟前`;
  if (sec < 86_400) return `${Math.floor(sec / 3600)} 小时前`;
  return `${Math.floor(sec / 86_400)} 天前`;
}

/** 活跃节点健康分（模板辅助：Active 条目收窄取 score，Inactive 恒 0）。 */
function nodeScore(n: P2pNodeMetaEntry): number {
  return 'active' in n.state ? n.state.active.score : 0;
}

/** 节点健康分徽章配色：≥80 绿 / 50-79 中性 / <50 黄（与节点发现页一致）。 */
function scoreClass(score: number): string {
  if (score >= 80) return 'node-score score-hi';
  if (score >= 50) return 'node-score score-mid';
  return 'node-score score-lo';
}

/** conversation_id → 未读数（当前用户）。 */
const unreadMap = ref<Record<string, number>>({});

// 输入与发送
const draft = ref('');
const sending = ref(false);

// —— 输入区扩展：@提及 + 📎附件（docs/IM_AGENTS_AND_FILES.md）——
/** 内置 AI 助手名（与服务端常量 NEXOS_ASSISTANT 对齐；@ 触发异步回复，3s 防风暴；@提及候选恒置顶）。 */
const NEXOS_ASSISTANT = 'NexOS助手';
/** 输入框（大厅/会话两个互斥视图共用一个 ref，同一时刻只挂载其一）。 */
const composerInputEl = ref<HTMLTextAreaElement | null>(null);
/** 隐藏文件选择框（📎 按钮触发 click）。 */
const fileInputEl = ref<HTMLInputElement | null>(null);
/** 单文件上限（与服务端 IM_FILE_MAX_BYTES 对齐；前端预检，超限不上传）。 */
const MAX_FILE_BYTES = 64 * 1024 * 1024;
/** 附件上传中（busy 态：禁 📎 与发送，防附件未就绪先发消息）。 */
const uploading = ref(false);
/** 上传完成待随下一条消息发送的附件（消息 body 只有一个 attachment 槽位，新上传覆盖旧暂存）。 */
const pendingAttachment = ref<ImFileUploadResp | null>(null);
/** 附件预检/上传错误（显示在输入框上方，随下一次附件操作清除）。 */
const attachError = ref('');
/** 正在下载的附件 file_id（busy 态：对应附件按钮禁用显示「下载中…」，空串 = 无）。 */
const downloadingFileId = ref('');
/** 附件下载超时：≤64MiB 信封 base64 与上传同量级（~85MB JSON），放宽到 5 分钟。 */
const IM_FILE_DOWNLOAD_TIMEOUT_MS = 300_000;

/**
 * 发送可用：有正文；或（仅群组会话）有待发附件可发纯附件消息——
 * 大厅/远程大厅/联邦大厅/直通消息通道均须正文（服务端 400；联邦与 DM
 * 通道不承载附件）。
 */
const canSend = computed(() => {
  if (inFedLobbyView.value) return !!draft.value.trim();
  if (inRemoteLobbyView.value) {
    return !!draft.value.trim() && activeRemoteState.value.phase === 'open';
  }
  if (inDmView.value) return !!draft.value.trim();
  return (
    !!draft.value.trim() ||
    (!!pendingAttachment.value && inGroupView.value && !uploading.value)
  );
});

/** 📎 → 打开文件选择框。 */
function pickFile(): void {
  attachError.value = '';
  fileInputEl.value?.click();
}

/**
 * 选择文件 → 前端预检 ≤64MiB → FileReader+base64 → POST /api/v1/im/files
 * （IM token；busy 态）→ 成功暂存待发附件条（随下一条消息发送）。
 */
async function onFileChosen(e: Event): Promise<void> {
  const input = e.target as HTMLInputElement;
  const file = input.files && input.files.length ? input.files[0] : null;
  input.value = ''; // 复位以允许重复选择同一文件
  if (!file) return;
  if (file.size > MAX_FILE_BYTES) {
    attachError.value = `文件超限：${fmtSize(file.size)} > 64 MiB（单文件上限）`;
    return;
  }
  uploading.value = true;
  attachError.value = '';
  try {
    const contentBase64 = await fileToBase64(file);
    pendingAttachment.value = await withIm((t) =>
      endpoints.imUploadFile(file.name, contentBase64, { imToken: t }),
    );
  } catch (e) {
    attachError.value = '附件上传失败：' + (e instanceof Error ? e.message : String(e));
  } finally {
    uploading.value = false;
  }
}

/** 移除待发附件（文件已落服务端，仅从待发条摘除，不影响已上传记录）。 */
function removePendingAttachment(): void {
  pendingAttachment.value = null;
}

/** File → base64（ArrayBuffer 分块转换：避免 String.fromCharCode 栈溢出与逐块 += 的 O(n²) 拼接）。 */
function fileToBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(new Error('读取本地文件失败'));
    reader.onload = () => {
      try {
        const bytes = new Uint8Array(reader.result as ArrayBuffer);
        const CHUNK = 0x8000; // 32KiB/块（spread 实参安全上限内）
        const parts: string[] = [];
        for (let i = 0; i < bytes.length; i += CHUNK) {
          parts.push(String.fromCharCode(...bytes.subarray(i, i + CHUNK)));
        }
        resolve(btoa(parts.join('')));
      } catch (e) {
        reject(e instanceof Error ? e : new Error(String(e)));
      }
    };
    reader.readAsArrayBuffer(file);
  });
}

// 搜索（顶部工具条回车触发；结果面板替换消息列表，清空返回正常流）
const searchQuery = ref('');
const searchResults = ref<ImMessageExt[]>([]);
const searching = ref(false);
/** 搜索结果模式：true 时消息列表区渲染结果面板而非正常消息流。 */
const searchMode = ref(false);
/** 本次搜索实际命中的会话范围（lobby 或群组 id），结果面板副标题用。 */
const searchScopeName = ref('');

// =============================================================================
// 会话模型（2026-08-23 重构）：本地大厅 / 联邦大厅 / 远程节点大厅是不同的东西，
// 各为左侧列表的独立会话项（不再用消息区内二级 Tab 切换）；群组/私聊不变。
// =============================================================================
/** 当前激活会话：my-lobby=我的大厅 / fed-lobby=联邦大厅（跨节点可写会话）/
 *  remote-lobby=某远程节点的大厅 / group=私聊·群组。 */
const activeView = ref<'my-lobby' | 'fed-lobby' | 'remote-lobby' | 'group'>('my-lobby');
/** 当前激活的远程节点大厅 NodeID（activeView==='remote-lobby' 时非空）。 */
const activeRemoteNodeId = ref('');
const inMyLobbyView = computed(() => activeView.value === 'my-lobby');
const inFedLobbyView = computed(() => activeView.value === 'fed-lobby');
const inRemoteLobbyView = computed(() => activeView.value === 'remote-lobby');
const inGroupView = computed(() => activeView.value === 'group');
/** 大厅类视图（我的/联邦两个大厅会话——共享心跳与在线列表，消息列表各自独立）。 */
const inLobbyView = computed(
  () => activeView.value === 'my-lobby' || activeView.value === 'fed-lobby',
);
/** 直通消息（dm-*）会话视图：文本-only（跨节点通道不承载附件），发送走 /im/dm。 */
const inDmView = computed(
  () => activeView.value === 'group' && !!currentGroup.value?.id.startsWith('dm-'),
);

// —— 我的大厅数据（GET /im/lobby*：本节点房间，消息只留本节点）——
const lobbyInfo = ref<ImLobbyInfo | null>(null);
const lobbyMessages = ref<ImMessageExt[]>([]);
const lobbyMembers = ref<ImLobbyMember[]>([]);
const lobbyError = ref('');
/** 「我的大厅」未读（大厅消息在会话非激活时到达累计；进入会话清零）。 */
const myLobbyUnread = ref(0);
/** 大厅心跳间隔（30s < 60s 在线窗口，保持自身在线 + 刷新在线列表）。 */
const LOBBY_HEARTBEAT_MS = 30000;
let lobbyHeartbeatTimer: ReturnType<typeof setInterval> | null = null;

// —— 联邦大厅数据（GET /im/fed-lobby*：跨节点共享频道 fed-lobby 独立会话，
//    与我的大厅完全隔离——所有连接节点的用户都可在此发言）——
const fedLobbyInfo = ref<ImLobbyInfo | null>(null);
const fedLobbyMessages = ref<ImMessageExt[]>([]);
const fedLobbyError = ref('');
/** 「联邦大厅」未读（fed-lobby 会话消息在会话非激活时到达累计；进入会话清零）。 */
const fedLobbyUnread = ref(0);

// —— 联邦接收开关（GET/POST /api/v1/im/federation，2026-08-23；入口=IM 设置面板）——
/** 开关状态：true 开 / false 关（大厅工具条变灰 + 暂停横幅）/ null 未知（未加载）。 */
const fedReceiveEnabled = ref<boolean | null>(null);
/** 切换请求进行中（防连点）。 */
const fedToggleBusy = ref(false);
/** 开关操作错误信息（成功即清除；暂停横幅 + 设置面板展示）。 */
const fedError = ref('');
/** 开关悬浮说明（当前状态 + 点击后效果；设置面板开关 title 用）。 */
const fedToggleTitle = computed(() =>
  fedReceiveEnabled.value === false
    ? '联邦接收已暂停：点击恢复接收其他节点发到联邦大厅的消息（本地消息与发送不受影响）'
    : '联邦接收已开启：点击暂停接收其他节点发到联邦大厅的消息（本地消息与发送不受影响）',
);

// —— IM 设置面板（2026-08-23：IM 联邦设置从系统设置页迁入本页 ⚙️ 按钮）——
/** 面板开关（顶部工具条 ⚙️ 按钮打开；沿用现有 modal 惯例）。 */
const showImSettings = ref(false);
// —— 选项卡（2026-08-24 用户反馈「做成一个选项卡，现在全在一个界面」：
//    弹窗分两个 Tab——settings=两个开关（联邦接收/允许浏览大厅），
//    agents=AI Agent 接入指示；内容不动，纯结构调整）——
/** 设置面板选项卡 key（settings=设置 / agents=AI Agent 接入）。 */
type ImSettingsTab = 'settings' | 'agents';
/** 当前激活选项卡（弹窗打开默认「设置」；面板用 v-show 切换——不丢各自内部状态）。 */
const imSettingsTab = ref<ImSettingsTab>('settings');
/** 大厅开放开关状态（GET/POST /api/v1/im/lobby/access，IM 或 admin token）；null=未知。 */
const lobbyAccess = ref<ImLobbyAccessStatus | null>(null);
/** 大厅开放切换请求进行中（防连点）。 */
const lobbyAccessBusy = ref(false);
/** 大厅开放开关读取/操作错误（设置面板内展示）。 */
const lobbyAccessError = ref('');

/** 打开 IM 设置面板：默认落在「设置」Tab；联邦接收状态由 startChat 常备（缺失补拉）；大厅开放/直通消息状态懒加载。 */
function openImSettings(): void {
  showImSettings.value = true;
  imSettingsTab.value = 'settings';
  if (fedReceiveEnabled.value === null) void loadFedReceiveStatus();
  if (!lobbyAccess.value) void loadLobbyAccess();
  if (!dmAccess.value) void loadDmAccess();
}

/** 关闭 IM 设置面板（开关即时生效，无需提交动作）。 */
function closeImSettings(): void {
  showImSettings.value = false;
}

// —— AI Agent 接入指示（2026-08-24：把外部 agent 接入协议速查收进设置面板
//    「AI Agent 接入」Tab（同日改选项卡结构前的 ③ 区块原样搬入）——
//    agent（或人转述）照抄即可 认证三步 → WS → 进群发言；端点/字段以
//    im.rs 实际实现为准，即 docs/IM_AGENTS_AND_FILES.md §6 的协议）——
/** 本节点 REST 基址（运行时取当前页面 origin，如 http://192.0.2.106:8558）。 */
const imAgentBase = computed(() => location.origin);
/** WS 基址（与 REST 同 host；https 页面自动用 wss，与 connectWs 同规则）。 */
const imAgentWsBase = computed(() =>
  location.protocol === 'https:' ? `wss://${location.host}` : `ws://${location.host}`,
);
/** 接入示例代码块 key（六块：认证 ×2 + WS + 进群 + 发言 + 补拉）。 */
type AgentSnippetKey = 'challenge' | 'verify' | 'ws' | 'join' | 'send' | 'backfill';
/**
 * 示例文本（$B 基址替换为运行时真实 origin；占位符用 &lt;尖括号&gt; 形式，
 * 经文本插值渲染——不在模板裸写尖括号，防 Vue 编译器当 HTML 标签）。
 */
const agentSnippets = computed<Record<AgentSnippetKey, string>>(() => {
  const b = imAgentBase.value;
  return {
    challenge: `curl -X POST ${b}/api/v1/im/auth/challenge -H 'Content-Type: application/json' -d '{"pubkey":"0x<66hex压缩公钥>"}'`,
    verify: `curl -X POST ${b}/api/v1/im/auth/verify -H 'Content-Type: application/json' -d '{"pubkey":"0x<66hex>","nonce":"<nonce>","signature":"<130hex>"}'`,
    ws: `${imAgentWsBase.value}/ws?user=<pubkey>&token=<IM token>`,
    join: `curl -X POST ${b}/api/v1/im/groups/<group_id>/join -H 'Authorization: Bearer <IM token>'`,
    send: `curl -X POST ${b}/api/v1/im/conversations/<group_id>/messages -H 'Authorization: Bearer <IM token>' -H 'Content-Type: application/json' -d '{"content":"agent 已进群","sender_kind":"agent"}'`,
    backfill: `curl -H 'Authorization: Bearer <IM token>' '${b}/api/v1/im/messages?conversation_id=<group_id>&after_id=<本地最后一条消息id>&limit=50'`,
  };
});
/** 刚复制成功的代码块 key（右上角按钮 ✓ 反馈 1.5s）。 */
const copiedAgentSnippet = ref<AgentSnippetKey | ''>('');
let agentSnippetTimer: ReturnType<typeof setTimeout> | undefined;

/** 复制接入示例代码块（安全上下文 Clipboard API，HTTP 非安全上下文回退
 *  execCommand——见 utils/clipboard.ts；失败 toast 提示）。 */
async function copyAgentSnippet(key: AgentSnippetKey): Promise<void> {
  const ok = await copyText(agentSnippets.value[key]);
  if (!ok) {
    toast.error('复制失败: 剪贴板不可用');
    return;
  }
  copiedAgentSnippet.value = key;
  clearTimeout(agentSnippetTimer);
  agentSnippetTimer = setTimeout(() => (copiedAgentSnippet.value = ''), 1500);
}

/** 群组速查列表（侧栏 groups 原样快照 + 按名排序便于查找；含 group 与 direct）。 */
const agentGroupList = computed(() =>
  [...groups.value].sort((a, b) => a.name.localeCompare(b.name, 'zh')),
);
/** 「开发组」判定（名字精确匹配——其他 agent 接入的目标群，行内高亮标注）。 */
function isDevGroup(g: ImGroup): boolean {
  return g.name === '开发组';
}
/** 刚复制成功的群组 id（行尾 ✓ 已复制 反馈 1.5s）。 */
const copiedGroupId = ref('');
let groupIdTimer: ReturnType<typeof setTimeout> | undefined;

/** 复制群组 id（点击速查行整行触发；失败 toast 提示）。 */
async function copyGroupId(id: string): Promise<void> {
  const ok = await copyText(id);
  if (!ok) {
    toast.error('复制失败: 剪贴板不可用');
    return;
  }
  copiedGroupId.value = id;
  clearTimeout(groupIdTimer);
  groupIdTimer = setTimeout(() => (copiedGroupId.value = ''), 1500);
}

// —— 一键复制完整接入说明（2026-08-24 用户反馈「这个选项卡的内容需要可以
//    完整复制，现在不能」——零散代码块按钮之外，顶部主按钮把整个 Tab 内容
//    拼成一份自包含纯文本，复制走可直接发给其他 AI agent 照做）——
/**
 * 完整接入说明纯文本（自包含、零上下文可读）：本 Tab 各节内容（同样的
 * 端点/字段，示例 curl 直取 agentSnippets）+ 动态拼入当前群组清单；
 * markdown 代码块组织。
 */
const agentFullGuide = computed<string>(() => {
  const b = imAgentBase.value;
  const s = agentSnippets.value;
  // 群组清单（动态：侧栏 groups 快照，每行「名称: group_id」；开发组标注目标群；
  // 空 → （暂无群组）——与速查列表同一数据源 agentGroupList）
  const groupLines = agentGroupList.value.length
    ? agentGroupList.value
        .map(
          (g) =>
            `- ${g.kind === 'group' ? '👥' : '👤'} ${g.name}: ${g.id}` +
            (isDevGroup(g) ? '（目标群组——开发组，agent 接入后进此群发言）' : ''),
        )
        .join('\n')
    : '（暂无群组）';
  return [
    '# NexOS IM · AI Agent 接入说明',
    '',
    '外部 agent 以自己的链上身份接入本 NexOS 节点的 IM（与人类同一套 REST/WS 通道，无 agent 专用 API）：认证三步换 token（24h，过期重跑）→ WS 实时收 → 进群发言。照抄以下步骤即可。',
    '',
    '## 基础信息',
    '',
    `- 本节点地址：${b}`,
    '- 身份：secp256k1 公钥（0x + 66 hex 压缩格式）——与 NexHub / 链上身份同款密钥体系，可复用同一把私钥',
    '',
    '## ① 三步认证（一次性）',
    '',
    '1. 申请挑战：',
    '',
    '```bash',
    s.challenge,
    '```',
    '',
    '→ 返回 {"nonce":"<64hex>","expires_in":60}（60s 内单次有效）。然后本地签名：对 nonce 做 sign(SHA-256(nonce))，取 65 字节 r||s||v 的 hex（130 字符，可带 0x 前缀）——JS 用 @noble/secp256k1 / Python 用 eth_keys / Rust 用 k256。',
    '',
    '2. 验签换 token：',
    '',
    '```bash',
    s.verify,
    '```',
    '',
    '→ 返回 {"token":"<64hex>","expires_in":86400}（24h 有效，过期重跑认证）——之后所有 REST 请求带 Authorization: Bearer <IM token>。',
    '',
    '## ② WS 实时收消息',
    '',
    '```text',
    s.ws,
    '```',
    '',
    '握手即验（token 有效且与 user 匹配，失败 401）。收到的帧（JSON 文本）：{"type":"im_message","conversation_id":"…","message":{…}} 或 im_lobby_message（大厅）——按 conversation_id 分发追加。',
    '',
    '## ③ 进群 + 发言（群组须先 join）',
    '',
    '```bash',
    s.join,
    '```',
    '',
    '```bash',
    s.send,
    '```',
    '',
    '- 群组非成员发言/补拉 403（join 一次即永久生效，member = token 反查 pubkey）',
    '- sender_kind:"agent" 让前端渲染 AI 徽标',
    '- 正文里的 @名字 由服务端自动解析进 mentions（无需自传）',
    '- 大厅发言改用 POST /api/v1/im/lobby/messages（GET /lobby 自动加入）',
    '',
    '断线补拉（WS 掉线期间的缺口用 after_id = 本地最后一条消息 id，按 id 去重升序追加）：',
    '',
    '```bash',
    s.backfill,
    '```',
    '',
    '大厅同语义：GET /api/v1/im/lobby/messages?after_id=',
    '',
    '## ④ 当前群组清单（名称: group_id）',
    '',
    groupLines,
    '',
    '## ⑤ 完整文档',
    '',
    '完整接入指南（含演示 agent 脚本参考、附件收发、@ 约定、消息推送通知 webhook）：docs/IM_AGENTS_AND_FILES.md（重点 §6 / §7）。',
    '',
  ].join('\n');
});
/** 完整说明刚复制成功（按钮变绿 ✓ 反馈 1.5s，同零散代码块模式）。 */
const copiedAgentGuide = ref(false);
let agentGuideTimer: ReturnType<typeof setTimeout> | undefined;

/** 一键复制完整接入说明（同零散代码块：剪贴板工具带回退；失败 toast 提示）。 */
async function copyAgentGuide(): Promise<void> {
  const ok = await copyText(agentFullGuide.value);
  if (!ok) {
    toast.error('复制失败: 剪贴板不可用');
    return;
  }
  copiedAgentGuide.value = true;
  clearTimeout(agentGuideTimer);
  agentGuideTimer = setTimeout(() => (copiedAgentGuide.value = false), 1500);
}

// —— 我的大厅 / 联邦大厅 消息分流（2026-08-23 用户纠正：两个互相隔离的独立
//    会话——我的大厅=lobbyMessages（本节点房间），联邦大厅=fedLobbyMessages
//    （fed-lobby 会话，独立拉取/WS 帧/补拉端点）；fed: 前缀过滤仅用于清掉
//    旧版落进 lobby 的历史联邦消息）——
/** 我的大厅消息（本节点用户/系统/agent + 远程直接进入本节点大厅的发言）。 */
const localLobbyMsgs = computed(() =>
  lobbyMessages.value.filter((m) => !m.sender_id.startsWith('fed:')),
);
/** 当前大厅类会话展示的消息列表（我的大厅/联邦大厅二选一）。 */
const activeLobbyFeedMsgs = computed(() =>
  inFedLobbyView.value ? fedLobbyMessages.value : localLobbyMsgs.value,
);

// =============================================================================
// 远程节点大厅会话（联邦互联，2026-08-23；节点发现页「💬 进入 IM」跳转目的地）
// /chat?node=<全量 NodeID>&name=<展示名> → 在左侧列表创建/激活独立会话项
// 「🏨 <name> 的大厅」（localStorage 持久化，下次打开 IM 还在）——在对方的
// 大厅里说话，而不是在自己的大厅里。
// =============================================================================
const route = useRoute();
/** localStorage 持久化键（会话项跨刷新保留；清浏览器数据即清空）。 */
const REMOTE_LOBBIES_STORAGE_KEY = 'os-im-remote-lobbies';
/** 远程节点大厅会话项。 */
interface RemoteLobbyEntry {
  /** 对方全量 NodeID（0x+66hex，P2P 寻址与 API :node_id 参数）。 */
  node_id: string;
  /** 展示名（?name= 传入，缺省 NodeID 短显）。 */
  label: string;
}
/** 已接入的远程节点大厅会话项列表（左侧「节点大厅」分区渲染）。 */
const remoteLobbies = ref<RemoteLobbyEntry[]>([]);

/** 单个远程大厅会话的运行态。 */
interface RemoteLobbyState {
  /** 对方大厅只读镜像（≤20 条脱敏消息，无附件内容）。 */
  messages: ImLobbyViewMessage[];
  /** 状态机：loading=查询中 / open=对方开放 / denied=对方未开放 /
   *  unreachable=对方无应答 / error=请求失败。 */
  phase: 'loading' | 'open' | 'denied' | 'unreachable' | 'error';
  /** 错误信息（denied/unreachable/error 态展示）。 */
  error: string;
  /** 镜像请求进行中（刷新按钮/输入框禁用）。 */
  loading: boolean;
}
/** node_id → 运行态（多会话各自独立，切换会话不丢已拉到的镜像）。 */
const remoteStates = ref<Record<string, RemoteLobbyState>>({});
/** 远程镜像轮询间隔（会话激活期间；对方无推送通道，轻量轮询）。 */
const REMOTE_LOBBY_POLL_MS = 10_000;
let remoteLobbyTimer: ReturnType<typeof setInterval> | null = null;

/** 取某节点运行态（无则惰性建默认态并写入响应式记录；返回响应式代理——
 *  经返回值改字段才能触发视图更新，勿直接持有新建的原始对象）。 */
function ensureRemoteState(nodeId: string): RemoteLobbyState {
  if (!remoteStates.value[nodeId]) {
    remoteStates.value = {
      ...remoteStates.value,
      [nodeId]: { messages: [], phase: 'loading', error: '', loading: false },
    };
  }
  return remoteStates.value[nodeId];
}

/** 当前激活远程会话的运行态（无激活时空态占位，模板只读不写）。 */
const activeRemoteState = computed<RemoteLobbyState>(
  () =>
    remoteStates.value[activeRemoteNodeId.value] ?? {
      messages: [],
      phase: 'loading',
      error: '',
      loading: false,
    },
);

/** 会话项展示名（entry.label 优先，缺省 NodeID 短显，如 node-113 / 0x1234…cdef）。 */
function remoteLabelOf(nodeId: string): string {
  return remoteLobbies.value.find((r) => r.node_id === nodeId)?.label || shortIdentity(nodeId);
}

/** localStorage 读取（异常静默降级为空列表）。 */
function loadRemoteLobbies(): void {
  try {
    const raw = window.localStorage.getItem(REMOTE_LOBBIES_STORAGE_KEY);
    const v = raw ? (JSON.parse(raw) as unknown) : null;
    remoteLobbies.value = Array.isArray(v)
      ? v.filter(
          (r): r is RemoteLobbyEntry =>
            !!r && typeof r === 'object' && typeof (r as RemoteLobbyEntry).node_id === 'string',
        )
      : [];
  } catch {
    remoteLobbies.value = [];
  }
}

/** localStorage 写回（异常静默——持久化失败只影响下次打开时的会话项列表）。 */
function saveRemoteLobbies(): void {
  try {
    window.localStorage.setItem(REMOTE_LOBBIES_STORAGE_KEY, JSON.stringify(remoteLobbies.value));
  } catch {
    /* 隐私模式等场景静默 */
  }
}

// —— 侧栏「节点大厅 / 节点」分区收起展开（localStorage 记忆，默认展开）——

/** 收起状态 localStorage 键（'1'=收起，'0'/缺省=展开）。 */
const NODE_LOBBIES_COLLAPSE_KEY = 'chat-collapse-node-lobbies';
const PEERS_COLLAPSE_KEY = 'chat-collapse-peers';

/** 「节点大厅」分区是否收起（标题常显；收起只 v-show 隐藏列表，轮询数据保留）。 */
const collapsedNodeLobbies = ref(false);
/** 「节点」分区（P2P 元数据自动发现列表）是否收起。 */
const collapsedPeers = ref(false);

/** localStorage 读取收起状态（异常静默降级为展开）。 */
function loadCollapseStates(): void {
  try {
    collapsedNodeLobbies.value = window.localStorage.getItem(NODE_LOBBIES_COLLAPSE_KEY) === '1';
    collapsedPeers.value = window.localStorage.getItem(PEERS_COLLAPSE_KEY) === '1';
  } catch {
    /* 隐私模式等场景静默 */
  }
}

/** 收起状态写回（异常静默——失败只影响下次打开时的默认展开）。 */
function saveCollapseState(key: string, collapsed: boolean): void {
  try {
    window.localStorage.setItem(key, collapsed ? '1' : '0');
  } catch {
    /* 隐私模式等场景静默 */
  }
}

/** 点击「节点大厅」标题行：toggle 收起/展开并持久化。 */
function toggleNodeLobbies(): void {
  collapsedNodeLobbies.value = !collapsedNodeLobbies.value;
  saveCollapseState(NODE_LOBBIES_COLLAPSE_KEY, collapsedNodeLobbies.value);
}

/** 点击「节点」标题行：toggle 收起/展开并持久化。 */
function togglePeers(): void {
  collapsedPeers.value = !collapsedPeers.value;
  saveCollapseState(PEERS_COLLAPSE_KEY, collapsedPeers.value);
}

/** 创建/更新远程大厅会话项（按 node_id 去重；label 空则保留旧值）并持久化。 */
function upsertRemoteLobby(nodeId: string, label: string): void {
  const idx = remoteLobbies.value.findIndex((r) => r.node_id === nodeId);
  if (idx >= 0) {
    const prev = remoteLobbies.value[idx];
    if (label && label !== prev.label) {
      remoteLobbies.value = remoteLobbies.value.map((r, i) =>
        i === idx ? { ...r, label } : r,
      );
      saveRemoteLobbies();
    }
    return;
  }
  remoteLobbies.value = [...remoteLobbies.value, { node_id: nodeId, label }];
  saveRemoteLobbies();
}

/** 移除远程大厅会话项（✕ 按钮）；移除的是激活会话则回到我的大厅。 */
function removeRemoteLobby(nodeId: string): void {
  remoteLobbies.value = remoteLobbies.value.filter((r) => r.node_id !== nodeId);
  delete remoteStates.value[nodeId];
  remoteStates.value = { ...remoteStates.value };
  saveRemoteLobbies();
  if (activeRemoteNodeId.value === nodeId) selectMyLobby();
}

/** 读取 ?node=/?name= 参数：创建/激活对应远程大厅会话项（进入页面与路由变化时调用）。 */
function handleNodeQuery(): void {
  const node = route.query.node;
  const name = route.query.name;
  if (typeof node === 'string' && /^0x[0-9a-fA-F]{66}$/.test(node)) {
    const label = typeof name === 'string' && name.trim() ? name.trim() : '';
    upsertRemoteLobby(node, label);
    selectRemoteLobby(node);
  }
}

/** 拉取对方大厅镜像（GET /lobby/remote/:id，IM token；经加密 P2P 通道）。 */
async function loadRemoteLobby(nodeId: string): Promise<void> {
  if (!nodeId) return;
  const st = ensureRemoteState(nodeId);
  st.loading = true;
  st.error = '';
  st.phase = 'loading';
  try {
    const r = await withIm((t) => endpoints.imLobbyRemoteGet(nodeId, undefined, { imToken: t }));
    if (r.public === true) {
      st.phase = 'open';
      st.messages = Array.isArray(r.messages) ? r.messages : [];
      if (activeRemoteNodeId.value === nodeId) void nextTick(scrollToBottom);
    } else if (r.public === false) {
      st.phase = 'denied';
      st.messages = [];
    } else {
      st.phase = 'unreachable';
      st.messages = [];
    }
  } catch (e) {
    st.phase = 'error';
    st.error = e instanceof Error ? e.message : String(e);
  } finally {
    st.loading = false;
  }
}

/** 认证就绪且当前会话是远程大厅时拉镜像（startChat / selectRemoteLobby 调用）。 */
function maybeLoadActiveRemote(): void {
  if (activeView.value !== 'remote-lobby' || !activeRemoteNodeId.value) return;
  if (authPhase.value !== 'ok') return; // 未认证先不拉（startChat 成功后补拉）
  void loadRemoteLobby(activeRemoteNodeId.value);
  startRemoteLobbyPolling();
}

function startRemoteLobbyPolling(): void {
  stopRemoteLobbyPolling();
  remoteLobbyTimer = setInterval(() => {
    if (activeView.value === 'remote-lobby' && activeRemoteNodeId.value) {
      void loadRemoteLobby(activeRemoteNodeId.value);
    } else {
      stopRemoteLobbyPolling(); // 会话切走即停（回来自动重拉）
    }
  }, REMOTE_LOBBY_POLL_MS);
}

function stopRemoteLobbyPolling(): void {
  if (remoteLobbyTimer !== null) {
    clearInterval(remoteLobbyTimer);
    remoteLobbyTimer = null;
  }
}

/** 远程镜像里的"自己"：sender_id = fed:<本节点>:<本 pubkey>（尾缀匹配）。 */
function isOwnRemoteMsg(m: ImLobbyViewMessage): boolean {
  return !!pubkey.value && m.sender_id.endsWith(pubkey.value);
}

/** 远程消息发送者短显（sender_name 优先，缺省 NodeID/pubkey 短显）。 */
function remoteSenderDisplay(m: ImLobbyViewMessage): string {
  return shortIdentity(m.sender_name || m.sender_id);
}

/** 远程状态提示文案（denied/unreachable/error 态列表区展示）。 */
const remotePhaseNotice = computed(() => {
  switch (activeRemoteState.value.phase) {
    case 'denied':
      return '对方节点未开放 IM 大厅（默认不允许浏览；对方可在其 IM 页右上 ⚙️ 设置 开放）';
    case 'unreachable':
      return '对方节点无应答——可能未直连或已离线，稍后刷新重试';
    case 'error':
      return `拉取对方大厅失败：${activeRemoteState.value.error}`;
    case 'loading':
      return '正在经 P2P 通道查询对方大厅…';
    default:
      return '';
  }
});

/** 远程发言（POST /lobby/remote/:id/messages，经 P2P 联邦送达对方大厅；
 *  fire-and-forget——落地与否以对方开放/接收开关为准，稍后拉镜像可见）。 */
async function sendRemoteLobbyMessage(): Promise<void> {
  const nodeId = activeRemoteNodeId.value;
  const text = draft.value.trim();
  if (!nodeId || !text || sending.value || uploading.value) return;
  if (remoteStates.value[nodeId]?.phase !== 'open') return;
  sending.value = true;
  const prevDraft = draft.value;
  draft.value = '';
  try {
    await withIm((t) => endpoints.imLobbyRemoteSend(nodeId, text, { imToken: t }));
    // 送达对端后经镜像拉回（异步落地，稍等半拍再刷新）
    window.setTimeout(() => {
      if (activeRemoteNodeId.value === nodeId) void loadRemoteLobby(nodeId);
    }, 600);
  } catch (e) {
    draft.value = prevDraft;
    const st = remoteStates.value[nodeId];
    if (st) {
      st.error = e instanceof Error ? e.message : String(e);
      if (e instanceof ApiError && e.status === 403) st.phase = 'denied'; // 对方刚关了开关
    }
  } finally {
    sending.value = false;
  }
}

// =============================================================================
// 加载函数
// =============================================================================
async function loadGroups(): Promise<void> {
  groupsLoading.value = true;
  groupsError.value = '';
  try {
    const v = await withIm((t) => endpoints.imGroups({ imToken: t }));
    groups.value = Array.isArray(v) ? v : [];
    if (currentGroup.value && !groups.value.some((g) => g.id === currentGroup.value!.id)) {
      currentGroup.value = null;
      messages.value = [];
    }
    // 刷新各对话未读数
    void refreshUnread();
  } catch (e) {
    groupsError.value = e instanceof Error ? e.message : String(e);
    groups.value = [];
  } finally {
    groupsLoading.value = false;
  }
}

async function refreshUnread(): Promise<void> {
  // 群组/私聊 + 直通消息会话（dm-*）一并刷新未读
  const ids = [...groups.value, ...dmEntries.value].map((g) => g.id);
  const results = await Promise.allSettled(
    ids.map((id) => withIm((t) => endpoints.imUnread(id, { imToken: t }))),
  );
  const next: Record<string, number> = {};
  results.forEach((r, i) => {
    if (r.status === 'fulfilled') {
      next[ids[i]] = r.value.unread;
    }
  });
  // 当前正在查看的对话未读恒为 0（已标记已读）
  if (currentGroup.value) next[currentGroup.value.id] = 0;
  unreadMap.value = next;
}

async function loadMessages(): Promise<void> {
  const gid = currentGroup.value?.id;
  if (!gid) return;
  if (!messages.value.length) messagesLoading.value = true;
  messagesError.value = '';
  try {
    const v = await withIm((t) => endpoints.imMessages(gid, { imToken: t }));
    messages.value = Array.isArray(v) ? v : [];
    syncCache(gid, messages.value);
    void nextTick(scrollToBottom);
    // 标记已读
    void markConversationRead();
  } catch (e) {
    messagesError.value = e instanceof Error ? e.message : String(e);
  } finally {
    messagesLoading.value = false;
  }
}

/**
 * 会话增量补拉（GET /api/v1/im/messages?conversation_id=&after_id=）：
 * 以本地（当前视图 + 缓存）最后一条消息 id 为游标，取严格晚于它的缺口，
 * 按 id 去重追加。用于 WS 重连补缺口与切换回曾看过的会话；无游标（本地
 * 没有任何消息）时退化为全量加载。失败静默（保留现有列表，不打断聊天）。
 */
async function loadMessagesIncremental(gid: string): Promise<void> {
  const cached = messagesCache.get(gid) ?? [];
  const list = currentGroup.value?.id === gid ? messages.value : cached;
  const lastId = list.length ? list[list.length - 1].id : null;
  if (!lastId) {
    if (currentGroup.value?.id === gid) await loadMessages();
    return;
  }
  try {
    const gap = await withIm((t) => endpoints.imMessagesAfter(gid, lastId, { imToken: t }));
    const incoming = Array.isArray(gap) ? gap : [];
    const next = appendDedup(list, incoming);
    if (next !== list) {
      if (currentGroup.value?.id === gid) {
        messages.value = next;
        void nextTick(scrollToBottom);
      }
      syncCache(gid, next);
    }
    if (currentGroup.value?.id === gid) void markConversationRead();
  } catch {
    // 补拉失败静默：403（非成员）/网络抖动时保留现有列表
  }
}

/** 把当前对话所有未读消息标记已读，并清零未读徽章。 */
async function markConversationRead(): Promise<void> {
  const gid = currentGroup.value?.id;
  if (!gid) return;
  const me = pubkey.value;
  const unread = messages.value.filter((m) => !m.read_by || !m.read_by.includes(me));
  await Promise.allSettled(
    unread.map((m) => withIm((t) => endpoints.imMarkRead(m.id, { imToken: t }))),
  );
  // 本地更新 read_by + 未读计数
  unread.forEach((m) => {
    if (!m.read_by) m.read_by = [];
    if (!m.read_by.includes(me)) m.read_by.push(me);
  });
  unreadMap.value = { ...unreadMap.value, [gid]: 0 };
}

// =============================================================================
// 大厅：加载 / 心跳 / 切换
// =============================================================================
async function loadLobby(): Promise<void> {
  lobbyError.value = '';
  try {
    // 并发拉大厅信息 + 最近消息；IM token Bearer 即心跳（新用户自动加入 + 欢迎广播）
    const [info, msgs] = await Promise.all([
      withIm((t) => endpoints.imLobby({ imToken: t })),
      withIm((t) => endpoints.imLobbyMessages({ imToken: t })),
    ]);
    lobbyInfo.value = info;
    lobbyMessages.value = Array.isArray(msgs) ? msgs : [];
    void nextTick(scrollToBottom);
    void loadLobbyMembers();
  } catch (e) {
    lobbyError.value = e instanceof Error ? e.message : String(e);
  }
}

async function loadLobbyMembers(): Promise<void> {
  try {
    const r = await withIm((t) => endpoints.imLobbyMembers({ imToken: t }));
    lobbyMembers.value = Array.isArray(r.members) ? r.members : [];
  } catch {
    // 静默失败：在线列表是辅助信息
  }
}

/**
 * 拉取联邦大厅数据（GET /im/fed-lobby + /im/fed-lobby/messages，IM token；
 * Bearer 即心跳 + 自动加入）。联邦大厅是与我的大厅完全隔离的独立会话——
 * 所有连接节点用户的发言都在这里（远端条目 sender_id=fed:<node>:<pubkey>）。
 */
async function loadFedLobby(): Promise<void> {
  fedLobbyError.value = '';
  try {
    const [info, msgs] = await Promise.all([
      withIm((t) => endpoints.imFedLobby({ imToken: t })),
      withIm((t) => endpoints.imFedLobbyMessages({ imToken: t })),
    ]);
    fedLobbyInfo.value = info;
    fedLobbyMessages.value = Array.isArray(msgs) ? msgs : [];
    void nextTick(scrollToBottom);
  } catch (e) {
    fedLobbyError.value = e instanceof Error ? e.message : String(e);
  }
}

/** 拉取联邦接收开关状态（静默失败——辅助信息，切换时以服务端返回为准）。 */
async function loadFedReceiveStatus(): Promise<void> {
  try {
    const r = await withIm((t) => endpoints.imFederationGet({ imToken: t }));
    fedReceiveEnabled.value = !!r.enabled;
  } catch {
    /* 保持未知态（按钮禁用），下次 startChat 重试 */
  }
}

/** 切换联邦接收开关：关闭=暂停接收远程大厅消息（本地消息与发送不受影响）。 */
async function toggleFedReceive(): Promise<void> {
  if (fedToggleBusy.value || fedReceiveEnabled.value === null) return;
  fedToggleBusy.value = true;
  fedError.value = '';
  const target = !fedReceiveEnabled.value;
  try {
    const r = await withIm((t) => endpoints.imFederationSet(target, { imToken: t }));
    fedReceiveEnabled.value = !!r.enabled;
    toast.success(r.enabled ? '🌐 联邦接收已开启' : '🌐 联邦接收已暂停（本地消息与发送不受影响）');
  } catch (e) {
    fedError.value = e instanceof Error ? e.message : String(e);
  } finally {
    fedToggleBusy.value = false;
  }
}

/** 读取大厅开放开关（IM 设置面板打开时懒加载；失败面板内提示）。 */
async function loadLobbyAccess(): Promise<void> {
  lobbyAccessError.value = '';
  try {
    const r = await withIm((t) => endpoints.imLobbyAccessGet({ imToken: t }));
    lobbyAccess.value = r;
  } catch (e) {
    lobbyAccess.value = null;
    lobbyAccessError.value = e instanceof Error ? e.message : String(e);
  }
}

/** 切换大厅开放开关（POST /lobby/access；成功以服务端返回为准 + 轻提示）。 */
async function toggleLobbyAccess(): Promise<void> {
  if (lobbyAccessBusy.value || !lobbyAccess.value) return;
  lobbyAccessBusy.value = true;
  lobbyAccessError.value = '';
  const target = !lobbyAccess.value.lobby_public;
  try {
    const r = await withIm((t) => endpoints.imLobbyAccessSet(target, { imToken: t }));
    lobbyAccess.value = r;
    toast.success(
      r.lobby_public ? '🏛 已允许其他节点浏览本机 IM 大厅' : '🏛 已关闭本机 IM 大厅浏览',
    );
  } catch (e) {
    lobbyAccessError.value = e instanceof Error ? e.message : String(e);
  } finally {
    lobbyAccessBusy.value = false;
  }
}

// =============================================================================
// 点对点直通消息 DM（2026-08-30）：大厅保留现状之外的独立私信通道——
// A 直接向某链上身份发私信，不经大厅广播，只有双方可见（服务端确定性
// dm-* 会话 + WS 定向推送；跨节点经 P2P 定向路由）。开关=IM 设置面板
// 「允许直通消息」（dm_open，开发阶段默认允许）。
// =============================================================================
/** 直通消息开放开关状态（GET/POST /api/v1/im/dm/access）；null=未知（未加载）。 */
const dmAccess = ref<ImDmAccessStatus | null>(null);
/** 直通消息开关切换请求进行中（防连点）。 */
const dmAccessBusy = ref(false);
/** 直通消息开关读取/操作错误（设置面板内展示）。 */
const dmAccessError = ref('');
/** 服务端 dm-* 会话（GET /im/conversations 过滤 dm- 前缀，映射为侧栏条目）。 */
const dmConversations = ref<ImGroup[]>([]);
/** 本地待建立的私信对象（peer pubkey → 展示名）：点「私信」即入列（服务端
 *  首条发送后才创建会话；此前用合成条目占位，id=dm-<peer>）。 */
const dmPendingPeers = ref<Record<string, string>>({});

/** 读取直通消息开关（设置面板打开时懒加载；失败面板内提示）。 */
async function loadDmAccess(): Promise<void> {
  dmAccessError.value = '';
  try {
    const r = await withIm((t) => endpoints.imDmAccessGet({ imToken: t }));
    dmAccess.value = r;
  } catch (e) {
    dmAccess.value = null;
    dmAccessError.value = e instanceof Error ? e.message : String(e);
  }
}

/** 切换直通消息开关（POST /im/dm/access；成功以服务端返回为准 + 轻提示）。 */
async function toggleDmAccess(): Promise<void> {
  if (dmAccessBusy.value || !dmAccess.value) return;
  dmAccessBusy.value = true;
  dmAccessError.value = '';
  const target = !dmAccess.value.dm_open;
  try {
    const r = await withIm((t) => endpoints.imDmAccessSet(target, { imToken: t }));
    dmAccess.value = r;
    toast.success(
      r.dm_open
        ? '✉️ 已允许他人向你发直通消息'
        : '✉️ 已关闭直通消息（他人发送将被拒绝；你发出的私信不受影响）',
    );
  } catch (e) {
    dmAccessError.value = e instanceof Error ? e.message : String(e);
  } finally {
    dmAccessBusy.value = false;
  }
}

/** 拉取 dm-* 会话（GET /im/conversations；服务端已按成员过滤——对方发起的
 *  私信也可见）。映射为侧栏 ImGroup 条目：kind='direct'、members=双方、
 *  name=对方短显（成员里非自己的那个 pubkey）。 */
async function loadDms(): Promise<void> {
  try {
    const list = await withIm((t) => endpoints.imConversations({ imToken: t }));
    const arr = Array.isArray(list) ? list : [];
    dmConversations.value = arr
      .filter((c) => c.id.startsWith('dm-') && Array.isArray(c.members) && c.members.length > 0)
      .map((c) => {
        const peer = c.members.find((m) => m !== pubkey.value) ?? c.members[0];
        return {
          id: c.id,
          name: dmPeerLabel(peer),
          kind: 'direct' as const,
          members: c.members,
        };
      });
    // 合成条目已被服务端会话取代（首条私信已发出）→ 清理占位
    const realPeers = new Set(dmConversations.value.map((g) => dmPeerOf(g)));
    const next: Record<string, string> = {};
    for (const [peer, name] of Object.entries(dmPendingPeers.value)) {
      if (!realPeers.has(peer)) next[peer] = name;
    }
    dmPendingPeers.value = next;
  } catch {
    // 静默降级：私信列表是辅助信息，失败不打断聊天（下次 WS 帧重拉）
  }
}

/** DM 会话/条目的「对方」pubkey（members 里非自己者）。 */
function dmPeerOf(g: ImGroup): string {
  const members = g.members ?? [];
  return members.find((m) => m !== pubkey.value) ?? members[0] ?? '';
}

/** 对方展示名：已知名字（点私信时带入的 sender_name）优先，缺省公钥短显。 */
function dmPeerLabel(peerPubkey: string): string {
  const known = dmPendingPeers.value[peerPubkey];
  return known || shortIdentity(peerPubkey);
}

/** 侧栏「对话」区的 DM 条目：服务端会话 + 本地待建占位（id=dm-<peer>，
 *  首条发送后服务端返回真实确定性 id 并自动切换）。 */
const dmEntries = computed<ImGroup[]>(() => {
  const out = [...dmConversations.value];
  for (const [peer, name] of Object.entries(dmPendingPeers.value)) {
    if (out.some((g) => dmPeerOf(g) === peer)) continue;
    out.push({
      id: `dm-${peer}`,
      name: name || shortIdentity(peer),
      kind: 'direct',
      members: peer === pubkey.value ? [peer] : [pubkey.value ?? '', peer],
    });
  }
  return out;
});

/** 发起/打开与某身份的私信（大厅消息「私信」按钮 / 在线成员头像点击）：
 *  登记 peer → 出现在「对话」区（无服务端会话则占位）→ 选中该会话。 */
function openDm(peerPubkey: string, peerName?: string): void {
  const peer = peerPubkey.trim();
  if (!peer || peer === pubkey.value) return;
  if (!dmPendingPeers.value[peer]) {
    dmPendingPeers.value = { ...dmPendingPeers.value, [peer]: peerName?.trim() || '' };
  } else if (peerName?.trim()) {
    dmPendingPeers.value = { ...dmPendingPeers.value, [peer]: peerName.trim() };
  }
  // 已有服务端会话 → 直接选中真实条目；否则选中占位条目
  const existing = dmConversations.value.find((g) => dmPeerOf(g) === peer);
  selectGroup(existing ?? { id: `dm-${peer}`, name: dmPeerLabel(peer), kind: 'direct', members: [pubkey.value ?? '', peer] });
}

/** 某大厅消息的发送者是否可发起私信（链上身份，非系统/助手/自己）。
 *  联邦大厅的远端身份（fed:<node>:<pubkey>）取尾缀原始 pubkey——跨节点
 *  路由由服务端按联邦大厅登记（pubkey→NodeID）自动定向。 */
function dmTargetOf(m: ImMessageExt): string {
  if (isSystemMsg(m)) return '';
  let pk = '';
  if (m.sender_id.startsWith('fed:')) {
    const rest = m.sender_id.slice(4);
    const idx = rest.indexOf(':');
    pk = idx > 0 ? rest.slice(idx + 1) : '';
  } else if (m.sender_id.startsWith('0x')) {
    pk = m.sender_id;
  }
  if (!pk || pk === pubkey.value) return '';
  return pk;
}

/** 大厅心跳：GET /lobby（Bearer）触碰 last_seen + 刷新在线成员（静默）。 */
async function lobbyHeartbeat(): Promise<void> {
  try {
    lobbyInfo.value = await withIm((t) => endpoints.imLobby({ imToken: t }));
    await loadLobbyMembers();
  } catch {
    /* 心跳失败不打扰聊天 */
  }
}

function startLobbyHeartbeat(): void {
  if (lobbyHeartbeatTimer !== null) return;
  lobbyHeartbeatTimer = setInterval(() => void lobbyHeartbeat(), LOBBY_HEARTBEAT_MS);
}

function stopLobbyHeartbeat(): void {
  if (lobbyHeartbeatTimer !== null) {
    clearInterval(lobbyHeartbeatTimer);
    lobbyHeartbeatTimer = null;
  }
}

// =============================================================================
// 选择会话（左侧列表项点击 → activeView 切换 + 各自数据流启停）
// =============================================================================
/** 切换前公共清理：关 @ 浮层 + 退出搜索结果面板（搜索范围随会话变化）。 */
function beforeSwitchView(): void {
  closeMention();
  exitSearchIfActive();
}

/** 「我的大厅」：本节点公共频道（首次/换回时增量补拉大厅消息）。 */
function selectMyLobby(): void {
  if (inMyLobbyView.value) return;
  beforeSwitchView();
  activeView.value = 'my-lobby';
  activeRemoteNodeId.value = '';
  currentGroup.value = null;
  stopRemoteLobbyPolling();
  myLobbyUnread.value = 0;
  if (!lobbyMessages.value.length) void loadLobby();
}

/** 「联邦大厅」：跨节点共享频道（可写会话，未读清零；首次/换回时增量补拉）。 */
function selectFedLobby(): void {
  if (inFedLobbyView.value) return;
  beforeSwitchView();
  activeView.value = 'fed-lobby';
  activeRemoteNodeId.value = '';
  currentGroup.value = null;
  stopRemoteLobbyPolling();
  fedLobbyUnread.value = 0;
  if (!fedLobbyMessages.value.length) void loadFedLobby();
}

/** 「<节点名> 的大厅」：远程节点大厅镜像 + 远程发言（激活期间轻量轮询）。 */
function selectRemoteLobby(nodeId: string): void {
  if (inRemoteLobbyView.value && activeRemoteNodeId.value === nodeId) return;
  beforeSwitchView();
  activeView.value = 'remote-lobby';
  activeRemoteNodeId.value = nodeId;
  currentGroup.value = null;
  ensureRemoteState(nodeId);
  maybeLoadActiveRemote();
}

function selectGroup(g: ImGroup): void {
  if (inGroupView.value && currentGroup.value?.id === g.id) return;
  beforeSwitchView();
  activeView.value = 'group';
  activeRemoteNodeId.value = '';
  stopRemoteLobbyPolling();
  currentGroup.value = g;
  const cached = messagesCache.get(g.id);
  if (cached && cached.length) {
    // 曾看过的会话：先回填缓存（无白屏），再 after_id 增量补新消息
    messages.value = [...cached];
    void scrollToBottom();
    void loadMessagesIncremental(g.id);
  } else {
    messages.value = [];
    void loadMessages();
  }
}

// =============================================================================
// WebSocket 实时推送（取代 3s 轮询；批次 2 起握手强制 ?user=pubkey&token=IM token）
// =============================================================================
let ws: WebSocket | null = null;
let wsReconnectTimer: ReturnType<typeof setTimeout> | null = null;
let wsClosedByUs = false;
/** 曾发生过一次非主动断线（onclose 且非我们 close）→ 下次 onopen 需补拉缺口。 */
let wsHadDrop = false;

/**
 * WS 断线重连成功后的离线补拉：断线期间服务端照常收消息并广播，但本端收不到
 * ——重连成功即以各列表本地最后一条消息 id 为游标调补拉端点把缺口追加进来
 * （按 id 去重，与可能已到的 WS 推送共存）：
 * - 当前会话（群组视图）：imMessagesAfter(gid, lastId) → 追加 + 滚动 + 标已读；
 * - 大厅：imLobbyMessagesAfter(lastLobbyId) → 追加（正在大厅视图则滚动）；
 * - 其它会话的缺口不逐个拉（省请求）：交给 refreshUnread 刷新未读徽章，
 *   切换回该会话时再走增量补拉。
 */
async function catchUpAfterReconnect(): Promise<void> {
  // 我的大厅补拉（为空则跳过，loadLobby 会全量拉）
  const lastLobbyId = lobbyMessages.value.length
    ? lobbyMessages.value[lobbyMessages.value.length - 1].id
    : null;
  if (lastLobbyId) {
    try {
      const gap = await withIm((t) =>
        endpoints.imLobbyMessagesAfter(lastLobbyId, { imToken: t }),
      );
      const incoming = Array.isArray(gap) ? gap : [];
      lobbyMessages.value = appendDedup(lobbyMessages.value, incoming);
      // 未在看的大厅会话累计未读徽章（fed: 前缀旧数据仍归联邦大厅语义）
      for (const m of incoming) {
        if (isFedMsg(m)) {
          if (!inFedLobbyView.value) fedLobbyUnread.value += 1;
        } else if (!inMyLobbyView.value) {
          myLobbyUnread.value += 1;
        }
      }
      if (inLobbyView.value) void nextTick(scrollToBottom);
    } catch {
      /* 大厅补拉失败静默 */
    }
  }
  // 联邦大厅补拉（独立 fed-lobby 会话，同语义）
  const lastFedLobbyId = fedLobbyMessages.value.length
    ? fedLobbyMessages.value[fedLobbyMessages.value.length - 1].id
    : null;
  if (lastFedLobbyId) {
    try {
      const gap = await withIm((t) =>
        endpoints.imFedLobbyMessagesAfter(lastFedLobbyId, { imToken: t }),
      );
      const incoming = Array.isArray(gap) ? gap : [];
      fedLobbyMessages.value = appendDedup(fedLobbyMessages.value, incoming);
      for (const m of incoming) {
        if (!isOwnMsg(m) && !inFedLobbyView.value) fedLobbyUnread.value += 1;
      }
      if (inFedLobbyView.value) void nextTick(scrollToBottom);
    } catch {
      /* 联邦大厅补拉失败静默 */
    }
  }
  // 当前会话补拉（群组视图才需要；大厅上面已处理，远程大厅走下面的刷新）
  if (inGroupView.value && currentGroup.value) {
    await loadMessagesIncremental(currentGroup.value.id);
  }
  // 激活中的远程大厅会话：无推送通道，直接刷一次镜像补缺口
  if (inRemoteLobbyView.value && activeRemoteNodeId.value) {
    void loadRemoteLobby(activeRemoteNodeId.value);
  }
  // 其它会话的离线缺口 → 未读徽章
  void refreshUnread();
}

/**
 * 建立带认证的 WS 连接：先 ensureAuthenticated（token 过期/将过期自动重走
 * 挑战-签名），再以 ?user=<pubkey>&token=<IM token> 握手（无 token 服务端 401）。
 * 重连成功（之前掉过线）自动触发离线补拉（catchUpAfterReconnect）。
 */
async function connectWs(): Promise<void> {
  if (!hasIdentity.value) return;
  if (ws && (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING)) {
    return;
  }
  let session;
  try {
    session = await ensureAuthenticated();
  } catch {
    scheduleReconnect();
    return;
  }
  wsClosedByUs = false;
  try {
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    ws = new WebSocket(
      `${proto}://${location.host}/ws` +
        `?user=${encodeURIComponent(session.pubkey)}` +
        `&token=${encodeURIComponent(session.token)}`,
    );
  } catch {
    scheduleReconnect();
    return;
  }
  ws.onopen = () => {
    if (wsHadDrop) {
      // 重连成功：补齐断线期间错过的大厅/当前会话消息
      wsHadDrop = false;
      void catchUpAfterReconnect();
    }
  };
  ws.onmessage = (e) => {
    let data: { type?: string; conversation_id?: string; message?: ImMessageExt };
    try {
      data = JSON.parse(e.data);
    } catch {
      return;
    }
    if (data.type === 'im_message' && data.message) {
      const msg = data.message;
      const cid = data.conversation_id || msg.conversation_id;
      // 新到达的 dm 会话（对方发起的私信）→ 刷新侧栏「对话」区列表
      if (cid && cid.startsWith('dm-') && !dmConversations.value.some((g) => g.id === cid)) {
        void loadDms();
      }
      if (cid && currentGroup.value && cid === currentGroup.value.id) {
        // 当前对话：追加 + 滚动 + 标记已读
        if (!messages.value.some((m) => m.id === msg.id)) {
          messages.value = [...messages.value, msg];
          syncCache(cid, messages.value);
          void nextTick(scrollToBottom);
          void markConversationRead();
        }
      } else if (cid) {
        // 其它对话：未读 +1（缓存里也补上，切换回去不缺消息）
        unreadMap.value = {
          ...unreadMap.value,
          [cid]: (unreadMap.value[cid] ?? 0) + 1,
        };
        const cached = messagesCache.get(cid);
        if (cached && !cached.some((m) => m.id === msg.id)) {
          syncCache(cid, [...cached, msg]);
        }
      }
    } else if (data.type === 'im_fed_lobby_message' && data.message) {
      // 联邦大厅消息（fed-lobby 会话：本地发言的广播回声 / 其他节点的联邦发言）
      // ——追加进独立的 fedLobbyMessages；正在看该会话则滚动到底，否则累计未读
      const msg = data.message;
      if (!fedLobbyMessages.value.some((m) => m.id === msg.id)) {
        fedLobbyMessages.value = [...fedLobbyMessages.value, msg];
      }
      if (inFedLobbyView.value) {
        void nextTick(scrollToBottom);
      } else if (!isOwnMsg(msg)) {
        fedLobbyUnread.value += 1;
      }
    } else if (data.type === 'im_lobby_message' && data.message) {
      // 大厅消息（用户消息 / 系统广播）：追加进 lobbyMessages（我的大厅＝本节点
      // 房间）；正在看的会话滚动到底并清未读，另一个大厅会话累计未读
      const msg = data.message;
      if (!lobbyMessages.value.some((m) => m.id === msg.id)) {
        lobbyMessages.value = [...lobbyMessages.value, msg];
      }
      if (isFedMsg(msg)) {
        // 旧版联邦消息（历史回声）：归联邦大厅未读
        if (inFedLobbyView.value) {
          void nextTick(scrollToBottom);
        } else {
          fedLobbyUnread.value += 1;
        }
      } else if (inMyLobbyView.value) {
        void nextTick(scrollToBottom);
      } else {
        myLobbyUnread.value += 1;
      }
      // 欢迎广播可能伴随新成员加入 → 刷新在线列表
      void loadLobbyMembers();
    }
  };
  ws.onclose = () => {
    ws = null;
    if (!wsClosedByUs) {
      wsHadDrop = true; // 非主动断线：重连成功后需要补拉
      scheduleReconnect();
    }
  };
  ws.onerror = () => {
    ws?.close();
  };
}

function scheduleReconnect(): void {
  if (wsReconnectTimer !== null) return;
  wsReconnectTimer = setTimeout(() => {
    wsReconnectTimer = null;
    void connectWs(); // 重连前 ensureAuthenticated 会检查 token 新鲜度
  }, 5000);
}

// =============================================================================
// 发送消息
// =============================================================================
/** 组装发消息扩展 body：有待发附件时带 attachment:{file_id}（人类客户端不带 sender_kind）。 */
function sendExtras(): ImSendExtras | undefined {
  return pendingAttachment.value
    ? { attachment: { file_id: pendingAttachment.value.file_id } }
    : undefined;
}

async function sendMessage(): Promise<void> {
  // 远程节点大厅会话：经 P2P 联邦发言到对方节点（文本-only）
  if (inRemoteLobbyView.value) return sendRemoteLobbyMessage();
  if (inMyLobbyView.value) return sendLobbyMessage();
  if (inFedLobbyView.value) return sendFedLobbyMessage();
  if (!inGroupView.value) return;
  const gid = currentGroup.value?.id;
  if (!gid) return;
  // 直通消息会话（dm-*）：走 POST /api/v1/im/dm 定向通道（不经大厅广播，
  // 只有双方可见；跨节点由服务端按登记路由自动定向）——文本-only
  if (gid.startsWith('dm-')) return sendDmMessage();
  const text = draft.value.trim();
  // 会话端点允许空正文：有附件即可发纯附件消息（服务端补落盘真值）
  if ((!text && !pendingAttachment.value) || sending.value || uploading.value) return;

  sending.value = true;
  const prevDraft = draft.value;
  const extras = sendExtras();
  draft.value = '';
  try {
    const msg = await withIm((t) => endpoints.imSendMessage(gid, text, { imToken: t }, extras));
    if (msg && msg.id) {
      // 服务端已广播；本地若已通过 WS 追加则去重，否则乐观追加
      if (!messages.value.some((m) => m.id === msg.id)) {
        messages.value = [...messages.value, msg];
      }
      syncCache(gid, messages.value);
    }
    pendingAttachment.value = null; // 附件已随消息发出，清空暂存
    void nextTick(scrollToBottom);
    void loadGroups();
  } catch (e) {
    draft.value = prevDraft; // 失败回滚草稿；附件保持暂存可重发
    messagesError.value = '发送失败：' + (e instanceof Error ? e.message : String(e));
  } finally {
    sending.value = false;
  }
}

/** 发送大厅消息（公共频道；sender 由服务端从 IM token 反查 pubkey；空白正文服务端 400）。 */
async function sendLobbyMessage(): Promise<void> {
  const text = draft.value.trim();
  if (!text || sending.value || uploading.value) return;
  sending.value = true;
  const prevDraft = draft.value;
  const extras = sendExtras();
  draft.value = '';
  try {
    const msg = await withIm((t) => endpoints.imLobbySend(text, { imToken: t }, extras));
    if (msg && msg.id && !lobbyMessages.value.some((m) => m.id === msg.id)) {
      lobbyMessages.value = [...lobbyMessages.value, msg];
    }
    pendingAttachment.value = null; // 附件已随消息发出，清空暂存
    void nextTick(scrollToBottom);
  } catch (e) {
    draft.value = prevDraft; // 失败回滚草稿；附件保持暂存可重发
    lobbyError.value = '发送失败：' + (e instanceof Error ? e.message : String(e));
  } finally {
    sending.value = false;
  }
}

/**
 * 发送联邦大厅消息（POST /im/fed-lobby/messages，IM token；sender 服务端反查
 * pubkey）。服务端本地落库 + P2P 广播全部已连接节点 + WS 广播本节点；联邦通道
 * 文本-only（不承载附件），须已加入（页面加载/会话切换时的 GET /fed-lobby 即加入）。
 */
async function sendFedLobbyMessage(): Promise<void> {
  const text = draft.value.trim();
  if (!text || sending.value || uploading.value) return;
  sending.value = true;
  const prevDraft = draft.value;
  draft.value = '';
  try {
    const msg = await withIm((t) => endpoints.imFedLobbySend(text, { imToken: t }));
    if (msg && msg.id && !fedLobbyMessages.value.some((m) => m.id === msg.id)) {
      fedLobbyMessages.value = [...fedLobbyMessages.value, msg];
    }
    void nextTick(scrollToBottom);
  } catch (e) {
    draft.value = prevDraft; // 失败回滚草稿
    fedLobbyError.value = '发送失败：' + (e instanceof Error ? e.message : String(e));
  } finally {
    sending.value = false;
  }
}

function onDraftEnter(e: KeyboardEvent): void {
  if (e.shiftKey) return;
  e.preventDefault();
  void sendMessage();
}

/**
 * 发送直通消息（POST /api/v1/im/dm，IM token；对方=当前 dm 会话成员里非
 * 自己者）。首条发送会把占位条目（id=dm-<peer>）切换为服务端返回的确定性
 * 会话 id；403=对方未开放直通消息（草稿回滚 + 错误提示）。跨节点 route=p2p
 * 时本地同样留档（服务端），消息立即可见（落地以对方开关为准）。
 */
async function sendDmMessage(): Promise<void> {
  const g = currentGroup.value;
  if (!g || !g.id.startsWith('dm-')) return;
  const peer = dmPeerOf(g);
  const text = draft.value.trim();
  if (!peer || !text || sending.value || uploading.value) return;
  sending.value = true;
  const prevDraft = draft.value;
  draft.value = '';
  try {
    const r = await withIm((t) => endpoints.imDmSend(peer, text, { imToken: t }));
    const msg = r?.message;
    if (msg && msg.id) {
      if (!messages.value.some((m) => m.id === msg.id)) {
        messages.value = [...messages.value, msg];
      }
      // 占位条目（dm-<peer>）→ 服务端确定性 id：登记真实会话并切换过去
      if (r.conversation_id && r.conversation_id !== g.id) {
        void loadDms();
        const real: ImGroup = {
          id: r.conversation_id,
          name: dmPeerLabel(peer),
          kind: 'direct',
          members: [pubkey.value ?? '', peer],
        };
        dmConversations.value = [
          ...dmConversations.value.filter((x) => x.id !== real.id),
          real,
        ];
        selectGroup(real);
      }
      syncCache(g.id, messages.value);
      void nextTick(scrollToBottom);
    }
  } catch (e) {
    draft.value = prevDraft; // 失败回滚草稿
    const hint =
      e instanceof ApiError && e.status === 403
        ? '对方未开放直通消息（对方可在其 IM 设置开启）'
        : e instanceof Error
          ? e.message
          : String(e);
    messagesError.value = '私信发送失败：' + hint;
  } finally {
    sending.value = false;
  }
}

// =============================================================================
// 搜索（GET /api/v1/im/search?q=&conversation_id=；范围=当前视图：大厅或会话）
// =============================================================================
async function runSearch(): Promise<void> {
  const q = searchQuery.value.trim();
  if (!q) {
    clearSearch();
    return;
  }
  // 范围跟随当前会话：我的大厅搜本节点大厅（lobby），联邦大厅搜 fed-lobby 会话，
  // 群组会话搜该会话；远程大厅镜像不支持搜索
  if (inRemoteLobbyView.value) return;
  const scope = inMyLobbyView.value
    ? 'lobby'
    : inFedLobbyView.value
      ? 'fed-lobby'
      : (currentGroup.value?.id ?? 'lobby');
  if (!inLobbyView.value && !currentGroup.value) return;
  searching.value = true;
  try {
    const r = await withIm((t) =>
      endpoints.imSearch(q, { imToken: t, conversationId: scope }),
    );
    searchResults.value = Array.isArray(r.results) ? r.results : [];
    searchScopeName.value = inLobbyView.value
      ? (inFedLobbyView.value ? '联邦大厅' : '我的大厅')
      : (currentGroup.value?.name || scope);
    searchMode.value = true;
  } catch {
    searchResults.value = [];
    searchMode.value = false;
  } finally {
    searching.value = false;
  }
}

function onSearchEnter(): void {
  void runSearch();
}

/** 切换视图（大厅↔会话）时退出搜索结果面板：搜索范围跟随当前视图，
 *  旧面板继续显示会误导；搜索词一并清空。 */
function exitSearchIfActive(): void {
  if (searchMode.value || searchQuery.value) clearSearch();
}

/** 清空搜索：退出结果面板，回到正常消息流。 */
function clearSearch(): void {
  searchQuery.value = '';
  searchResults.value = [];
  searchMode.value = false;
  searchScopeName.value = '';
}

/** 点击搜索结果：跳转到消息所在会话（fed-lobby 会话→联邦大厅，fed: 旧数据与
 *  其余大厅消息→我的大厅，群组→对应会话）。 */
async function jumpToMessage(m: ImMessageExt): Promise<void> {
  if (m.conversation_id === 'fed-lobby') {
    selectFedLobby();
  } else if (
    m.conversation_id === 'lobby' ||
    !groups.value.some((g) => g.id === m.conversation_id)
  ) {
    if (isFedMsg(m)) selectFedLobby();
    else selectMyLobby();
  } else {
    const target = groups.value.find((g) => g.id === m.conversation_id);
    if (target) selectGroup(target);
  }
  clearSearch();
}

// =============================================================================
// 自动滚动到底部
// =============================================================================
const messageListEl = ref<HTMLElement | null>(null);

function scrollToBottom(): void {
  const el = messageListEl.value;
  if (!el) return;
  el.scrollTop = el.scrollHeight;
}

// =============================================================================
// 创建群组对话框
// =============================================================================
const showCreateGroup = ref(false);
const newGroupName = ref('');
const createGroupSubmitting = ref(false);
const createGroupMsg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);

function openCreateGroup(): void {
  newGroupName.value = '';
  createGroupMsg.value = null;
  showCreateGroup.value = true;
}
function closeCreateGroup(): void {
  if (createGroupSubmitting.value) return;
  showCreateGroup.value = false;
}
async function submitCreateGroup(): Promise<void> {
  const name = newGroupName.value.trim();
  if (!name) {
    createGroupMsg.value = { kind: 'err', text: '请填写群组名' };
    return;
  }
  createGroupSubmitting.value = true;
  createGroupMsg.value = { kind: 'info', text: '创建中…' };
  try {
    await withIm((t) => endpoints.imCreateGroup(name, [pubkey.value], { imToken: t }));
    showCreateGroup.value = false;
    await loadGroups();
  } catch (e) {
    createGroupMsg.value = {
      kind: 'err',
      text: '创建失败：' + (e instanceof Error ? e.message : String(e)),
    };
  } finally {
    createGroupSubmitting.value = false;
  }
}

// =============================================================================
// 身份卡：生成（一次性私钥展示）/ 导入 / 切换清除
// =============================================================================
const showGenKey = ref(false);
const generatedPrivkey = ref(''); // 一次性明文展示（关闭即清）
const generatedPubkey = ref('');
const privkeyCopied = ref(false);

const showImportKey = ref(false);
const importKeyInput = ref('');
const importKeyMsg = ref<{ kind: 'err' | 'info'; text: string } | null>(null);

/** 生成新身份并弹一次性私钥展示（可跳过抄存）。 */
function startGenerateIdentity(): void {
  try {
    const id = generateIdentity();
    generatedPrivkey.value = id.privkeyHex;
    generatedPubkey.value = id.pubkey;
    privkeyCopied.value = false;
    showGenKey.value = true;
  } catch (e) {
    authPhase.value = 'error';
    authError.value = '生成身份失败：' + (e instanceof Error ? e.message : String(e));
  }
}

async function copyGeneratedPrivkey(): Promise<void> {
  privkeyCopied.value = await copyText(generatedPrivkey.value);
}

/** 关闭一次性私钥展示（我已抄存 / 跳过 都关闭）→ 立即自动认证。 */
function closeGenKey(): void {
  showGenKey.value = false;
  generatedPrivkey.value = '';
  generatedPubkey.value = '';
  void bootstrapAuth();
}

function openImportKey(): void {
  importKeyInput.value = '';
  importKeyMsg.value = null;
  showImportKey.value = true;
}
function closeImportKey(): void {
  showImportKey.value = false;
}

/** 导入私钥（本地校验格式）→ 关闭弹窗 → 自动认证。 */
function submitImportKey(): void {
  try {
    importIdentity(importKeyInput.value);
    showImportKey.value = false;
    importKeyInput.value = '';
    void bootstrapAuth();
  } catch (e) {
    importKeyMsg.value = { kind: 'err', text: e instanceof Error ? e.message : String(e) };
  }
}

/** 切换身份/清除：停数据流 + 清本机身份（私钥+token），回到初始化卡。 */
function switchIdentity(): void {
  const ok = window.confirm(
    `清除本机 IM 身份（${shortPubkey.value || pubkey.value}）？\n` +
      '私钥若未抄存，该身份将无法找回。清除后需重新生成或导入。',
  );
  if (!ok) return;
  teardownChat();
  clearIdentity();
  authPhase.value = 'no-identity';
  authError.value = '';
}

// =============================================================================
// 展示辅助
// =============================================================================
/** pubkey 缩略（0x1a2b…c3d4e5f6）。 */
const shortPubkey = computed(() => {
  const pk = pubkey.value;
  return pk ? `${pk.slice(0, 10)}…${pk.slice(-8)}` : '';
});
/** EVM 展示名后 8 位（display_name = 0x + 40 hex）。 */
const displaySuffix = computed(() => {
  const dn = imDisplayName.value;
  return dn ? dn.slice(-8) : '';
});
/** 身份卡上的认证状态文案。 */
const authPhaseText = computed(() => {
  if (authPhase.value === 'ok') return '已认证';
  if (authPhase.value === 'authing' || imAuthenticating.value) return '认证中…';
  return '未认证';
});

function isOwnMsg(m: ImMessageExt): boolean {
  return !!pubkey.value && m.sender_id === pubkey.value;
}
function isSystemMsg(m: ImMessageExt): boolean {
  return m.msg_type === 'system' || m.sender_id === 'system';
}
/** agent 消息（sender_kind 展示层自声明 → 🤖徽章/紫描边；不作权限依据）。 */
function isAgentMsg(m: ImMessageExt): boolean {
  return m.sender_kind === 'agent';
}
/** 联邦远程消息（sender_id 以 fed: 开头——经 os-p2p 从其他 NexOS 节点同步）。 */
function isFedMsg(m: ImMessageExt): boolean {
  return m.sender_id.startsWith('fed:');
}
/** 远程消息来源节点名（`fed:<node>:<原 pubkey>` → `<node>`，如 node-106）。 */
function fedNodeOf(m: ImMessageExt): string {
  const rest = m.sender_id.slice(4); // 去掉 "fed:"
  const idx = rest.indexOf(':');
  return idx > 0 ? rest.slice(0, idx) : rest;
}
/** 消息头像 identicon（sender_id：人类=公钥、助手=agent:nexos-assistant——同身份恒同图）。 */
function msgIdenticon(m: ImMessageExt): string {
  return identiconSvg(m.sender_id, 30);
}
function senderDisplay(m: ImMessageExt): string {
  // 0x 身份（人类 sender_name=派生 EVM 地址，兜底 sender_id=公钥）→
  // `0x**…后四位` 短显；普通名（如 NexOS助手）与 agent:nexos-assistant 原样
  return shortIdentity(m.sender_name || m.sender_id);
}
/** 被回复消息的内容摘要（在当前消息列表里查；查不到则只显示“原消息”）。 */
function replyPreview(m: ImMessageExt): string {
  if (!m.reply_to) return '';
  const target = messages.value.find((x) => x.id === m.reply_to);
  if (!target) return '原消息';
  const txt = target.content || '';
  return txt.length > 40 ? `${txt.slice(0, 40)}…` : txt;
}

// —— @提及高亮：mentions 有值时把正文中对应 @xxx 段渲染为 accent 色 span ——
/** 正文切段结果：mention 段高亮，其余原样（保留 pre-wrap 换行）。 */
interface ContentSeg {
  text: string;
  mention: boolean;
}

/** 正则元字符转义（mentions 名字拼进 pattern 前消毒）。 */
function escapeRegExp(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

/** 把正文按 mentions 切段（@名字 命中段高亮；mentions 为空退化为单段纯文本）。 */
function contentSegments(m: ImMessageExt): ContentSeg[] {
  const names = (m.mentions ?? []).filter((n) => !!n);
  if (!names.length || !m.content) return [{ text: m.content, mention: false }];
  const pattern = new RegExp(`@(?:${names.map(escapeRegExp).join('|')})`, 'g');
  const segs: ContentSeg[] = [];
  let last = 0;
  for (const mt of m.content.matchAll(pattern)) {
    const idx = mt.index ?? 0;
    if (idx > last) segs.push({ text: m.content.slice(last, idx), mention: false });
    segs.push({ text: mt[0], mention: true });
    last = idx + mt[0].length;
  }
  if (last < m.content.length) segs.push({ text: m.content.slice(last), mention: false });
  return segs;
}

/** 人类可读文件大小（B/KiB/MiB/GiB）。 */
function fmtSize(bytes: number | null | undefined): string {
  if (typeof bytes !== 'number' || !Number.isFinite(bytes) || bytes < 0) return '';
  if (bytes < 1024) return `${bytes} B`;
  const kb = bytes / 1024;
  if (kb < 1024) return `${kb.toFixed(kb < 10 ? 1 : 0)} KiB`;
  const mb = kb / 1024;
  if (mb < 1024) return `${mb.toFixed(mb < 10 ? 1 : 0)} MiB`;
  return `${(mb / 1024).toFixed(1)} GiB`;
}

/** GET /api/v1/im/files/:file_id 响应信封（im.rs ImFileDownload：网关恒 JSON，回不了裸流）。 */
interface ImFileDownloadEnvelope {
  file_id: string;
  filename: string;
  size_bytes: number;
  mime_type: string;
  /** 恒为 "base64"。 */
  encoding: string;
  content_base64: string;
}

/**
 * fetch 附件下载信封（token 已含在 imFileUrl 的 query；5 分钟 AbortController）。
 * fetch 对 4xx 不 reject：401 转 ApiError 抛出（交 withIm 清 token 重认证重试
 * 一次）；其余非 2xx 解析 body.error 后抛错。超时/中断统一转中文提示。
 */
async function fetchImFileEnvelope(fileId: string, imToken: string): Promise<ImFileDownloadEnvelope> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), IM_FILE_DOWNLOAD_TIMEOUT_MS);
  try {
    const resp = await fetch(endpoints.imFileUrl(fileId, imToken), {
      signal: controller.signal,
    });
    if (resp.status === 401) {
      throw new ApiError('IM token 失效', { status: 401, path: resp.url });
    }
    if (!resp.ok) {
      let detail = '';
      try {
        detail = ((await resp.json()) as { error?: string }).error ?? '';
      } catch {
        /* 非 JSON body：仅报状态码 */
      }
      throw new ApiError(`HTTP ${resp.status}${detail ? ` — ${detail}` : ''}`, {
        status: resp.status,
        path: resp.url,
      });
    }
    return (await resp.json()) as ImFileDownloadEnvelope;
  } catch (e) {
    if (e instanceof DOMException && e.name === 'AbortError') {
      throw new Error('下载超时（5 分钟）或被中断');
    }
    throw e;
  } finally {
    clearTimeout(timer);
  }
}

/** Blob → ObjectURL → `<a download>` 点击另存（延后 revoke：立即回收在部分浏览器会截断下载）。 */
function saveBlobAs(blob: Blob, filename: string): void {
  const objUrl = URL.createObjectURL(blob);
  try {
    const a = document.createElement('a');
    a.href = objUrl;
    a.download = filename;
    document.body.appendChild(a);
    a.click();
    a.remove();
  } finally {
    setTimeout(() => URL.revokeObjectURL(objUrl), 10_000);
  }
}

/**
 * 点击消息附件「下载」→ 人类浏览器另存路径（busy 态防重复点击）。
 *
 * 直链回的是 base64-JSON 信封而非字节流——window.open 直开人类只会看到
 * JSON 文本，故前端解码：fetch 信封 → content_base64 → Blob（mime 用信封
 * mime_type，缺省 application/octet-stream）→ ObjectURL + `<a download=信封
 * filename>` 另存。双通道语义：agent 等 API 消费方仍可直连该信封 URL 自行
 * 解码（token query 形态与响应结构均不变），此处仅为人类下载体验兜底。
 */
async function openAttachment(a: ImMessageAttachment | null | undefined): Promise<void> {
  if (!a || !a.file_id || downloadingFileId.value) return;
  downloadingFileId.value = a.file_id;
  attachError.value = '';
  try {
    const env = await withIm((t) => fetchImFileEnvelope(a.file_id, t));
    const blob = new Blob([base64ToBytes(env.content_base64)], {
      type: env.mime_type || 'application/octet-stream',
    });
    saveBlobAs(blob, env.filename || a.filename || a.file_id);
  } catch (e) {
    attachError.value = '附件下载失败：' + (e instanceof Error ? e.message : String(e));
  } finally {
    downloadingFileId.value = '';
  }
}
function fmtTime(iso: string | undefined): string {
  if (!iso) return '';
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const now = new Date();
  const sameDay =
    d.getFullYear() === now.getFullYear() &&
    d.getMonth() === now.getMonth() &&
    d.getDate() === now.getDate();
  const pad = (n: number) => (n < 10 ? `0${n}` : String(n));
  const hm = `${pad(d.getHours())}:${pad(d.getMinutes())}`;
  if (sameDay) return hm;
  return `${pad(d.getMonth() + 1)}/${pad(d.getDate())} ${hm}`;
}
function unreadOf(gid: string): number {
  return unreadMap.value[gid] ?? 0;
}

/** 大厅在线成员（60s 心跳窗口内活跃）。 */
const lobbyOnlineMembers = computed(() => lobbyMembers.value.filter((m) => m.online));
function lobbyMemberName(m: ImLobbyMember): string {
  return m.display_name || m.user_id;
}
/** 成员头像（identicon：user_id=公钥，同身份恒同图，与消息气泡头像一致）。 */
function lobbyMemberIdenticon(m: ImLobbyMember): string {
  return identiconSvg(m.user_id, 28);
}
/** 悬浮提示：展示名 + 公钥短显（`0x**…后四位`）。 */
function lobbyMemberTitle(m: ImLobbyMember): string {
  return `${lobbyMemberName(m)} · ${shortIdentity(m.user_id)}`;
}

// =============================================================================
// @ 成员选择器（mention picker）：输入 `@词` 触发浮层 → 候选列表 → 键盘/点击
// 精确补全（大厅与会话两个互斥 composer 共用一份状态与逻辑）
//
// 已知限制：外部 agent 是否响应 @ 由其自身实现决定——mentions 是服务端纯
// 文本解析（`@<名字>`，无注册/路由；agent 侧自行匹配 WS 帧 message.mentions
// 是否含自己的名字，协议 docs/IM_AGENTS_AND_FILES.md §6.4）。本 UI 只保证把
// 正在输入的 @词 精确补全成完整名（尾随空格分隔），使服务端 mentions 解析
// 命中该名字；对方收不收、回不回是它自己的事。
// =============================================================================
/** 触发检测：光标前文本以 `@+名字串` 结尾即浮层激活（名字串可为空 = 刚敲下 @）。
 *  名字字符集与服务端 im.rs is_mention_char 对齐：CJK 基本区 + ASCII 字母数字 + `_`/`-`。 */
const MENTION_TRIGGER_RE = /@([一-龥A-Za-z0-9_-]*)$/u;
/** 浮层最多展示项数（更多靠继续输入 @词 前缀过滤）。 */
const MENTION_MAX_ITEMS = 8;
/** 内置助手 sender_id（与消息气泡 identicon 同源：agent:nexos-assistant）。 */
const ASSISTANT_SENDER_ID = 'agent:nexos-assistant';

/** 浮层候选项。 */
interface MentionCandidate {
  /** 身份串（identicon 种子：公钥 / agent:nexos-assistant）。 */
  id: string;
  /** 补全插入的完整名字（0x 身份即 EVM 地址全文——mentions 纯文本解析需精确命中）。 */
  name: string;
  /** 浮层展示名（0x 短显 `0x**…b58a`，普通名原样）。 */
  label: string;
  /** 🤖 徽章：内置助手恒真；其余按近期消息 sender_kind=agent 判定（展示语义同气泡）。 */
  agent: boolean;
}

/** 浮层是否打开。 */
const mentionActive = ref(false);
/** 正在输入的 @词（@ 后至光标）。 */
const mentionQuery = ref('');
/** 键盘选中项（mentionFiltered 内下标）。 */
const mentionIndex = ref(0);
/** 浮层根元素（「点击外部关闭」判定用；v-if 关闭时为 null）。 */
const mentionPopupEl = ref<HTMLElement | null>(null);

/** 当前会话消息里 sender_kind=agent 的发送者集合（浮层 🤖 徽章数据源）。 */
const agentSenderIds = computed<Set<string>>(() => {
  const s = new Set<string>();
  const src = inFedLobbyView.value
    ? fedLobbyMessages.value
    : inLobbyView.value
      ? lobbyMessages.value
      : messages.value;
  for (const m of src) {
    if (m.sender_kind === 'agent' && m.sender_id) s.add(m.sender_id);
  }
  return s;
});

/**
 * 候选全集：NexOS助手 置顶 + 当前上下文成员（大厅=在线成员列表；会话=已知
 * 发送者最新在前 + 会话成员 pubkey 兜底），按 id/名字双重去重，剔除自己与
 * 系统发送者。
 */
const mentionCandidates = computed<MentionCandidate[]>(() => {
  const out: MentionCandidate[] = [];
  const seenId = new Set<string>();
  const seenName = new Set<string>();
  const push = (id: string, name?: string): void => {
    const n = (name ?? '').trim() || id;
    if (!n || id === pubkey.value || seenId.has(id) || seenName.has(n)) return;
    seenId.add(id);
    seenName.add(n);
    out.push({ id, name: n, label: shortIdentity(n), agent: agentSenderIds.value.has(id) });
  };
  // ① 内置助手置顶（🤖 恒真，不依赖近期消息）
  seenId.add(ASSISTANT_SENDER_ID);
  seenName.add(NEXOS_ASSISTANT);
  out.push({
    id: ASSISTANT_SENDER_ID,
    name: NEXOS_ASSISTANT,
    label: NEXOS_ASSISTANT,
    agent: true,
  });
  // ② 大厅会话（我的大厅）：在线成员（60s 心跳窗口列表，现有数据）
  if (inLobbyView.value) {
    for (const m of lobbyOnlineMembers.value) push(m.user_id, m.display_name);
    return out;
  }
  // ③ 会话：已知发送者（最新在前，sender_name 优先）→ 成员列表 pubkey 兜底
  for (let i = messages.value.length - 1; i >= 0; i--) {
    const m = messages.value[i];
    if (isSystemMsg(m)) continue;
    push(m.sender_id, m.sender_name);
  }
  for (const uid of currentGroup.value?.members ?? []) push(uid);
  return out;
});

/** 浮层列表：@词 前缀过滤（不区分大小写；中文直接前缀匹配），最多 8 项。 */
const mentionFiltered = computed<MentionCandidate[]>(() => {
  const q = mentionQuery.value.toLowerCase();
  const list = q
    ? mentionCandidates.value.filter((c) => c.name.toLowerCase().startsWith(q))
    : mentionCandidates.value;
  return list.slice(0, MENTION_MAX_ITEMS);
});

/** 候选列表缩短（继续输入/消息到达）时选中下标回位。 */
watch(mentionFiltered, (list) => {
  if (mentionIndex.value >= list.length) mentionIndex.value = 0;
});

/** 关闭浮层（Esc / 点击外部 / 补全后 / 切换视图调用）。 */
function closeMention(): void {
  mentionActive.value = false;
  mentionQuery.value = '';
  mentionIndex.value = 0;
}

/**
 * 触发检测（@input / 点击 / 光标移动 keyup 后调用）：光标前文本命中
 * MENTION_TRIGGER_RE 即激活浮层。直接读元素值与选区——同一 input 事件里
 * v-model 的同步顺序不保证先于本 handler。@词 变化才重置选中项（否则
 * 浮层内的方向键 keyup 会把选中项弹回第一项）。
 */
function updateMentionState(): void {
  // 联邦大厅/远程大厅不提供 @ 浮层（联邦频道成员跨节点无解析表，远程通道文本-only）
  if (inFedLobbyView.value || inRemoteLobbyView.value) {
    closeMention();
    return;
  }
  const el = composerInputEl.value;
  if (!el || el.selectionStart === null) {
    closeMention();
    return;
  }
  const m = MENTION_TRIGGER_RE.exec(el.value.slice(0, el.selectionStart));
  if (!m) {
    closeMention();
    return;
  }
  if (!mentionActive.value || m[1] !== mentionQuery.value) mentionIndex.value = 0;
  mentionQuery.value = m[1];
  mentionActive.value = true;
}

/**
 * 补全：把光标前的 `@词` 替换为 `@完整名 `（尾空格与后续文字分隔），浮层
 * 关闭，光标移到名字后（尾空格之后，便于继续输入正文）。名字取候选精确
 * 全文 → 服务端 mentions 解析命中（外部 agent 侧自匹配，见本节已知限制）。
 */
function applyMention(idx: number): void {
  const c = mentionFiltered.value[idx];
  const el = composerInputEl.value;
  if (!c || !el || el.selectionStart === null) {
    closeMention();
    return;
  }
  const pos = el.selectionStart;
  const m = MENTION_TRIGGER_RE.exec(el.value.slice(0, pos));
  if (!m) {
    closeMention();
    return;
  }
  const at = pos - m[1].length - 1; // '@' 下标
  const insert = `@${c.name} `;
  draft.value = el.value.slice(0, at) + insert + el.value.slice(pos);
  closeMention();
  void nextTick(() => {
    el.focus();
    const caret = at + insert.length;
    el.setSelectionRange(caret, caret);
  });
}

/**
 * composer 键盘总入口（大厅/会话共用）：浮层开着且有候选时 ↑↓ 循环移动
 * 选中项、Enter（非 Shift）/Tab 补全、Esc 只关浮层（无候选时浮层本就隐藏，
 * Enter 落回正常发送）；其余按键走原逻辑（Enter 发送 / Shift+Enter 换行）。
 * IME 组合中不劫持任何键——Enter 此时是确认候选字，也不应触发发送（原
 * @keydown.enter 无此守卫，中文输入 @名字 依赖它）。
 */
function onComposerKeydown(e: KeyboardEvent): void {
  if (e.isComposing || e.keyCode === 229) return;
  if (mentionActive.value && mentionFiltered.value.length) {
    if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
      e.preventDefault();
      const n = mentionFiltered.value.length;
      if (n) {
        mentionIndex.value = (mentionIndex.value + (e.key === 'ArrowDown' ? 1 : -1) + n) % n;
      }
      return;
    }
    if (e.key === 'Tab' || (e.key === 'Enter' && !e.shiftKey)) {
      e.preventDefault();
      applyMention(mentionIndex.value);
      return;
    }
    if (e.key === 'Escape') {
      e.preventDefault();
      closeMention();
      return;
    }
  }
  if (e.key === 'Enter') onDraftEnter(e);
}

/** 光标移动键（←→/Home/End）keyup 后重估触发状态；Esc 的 keyup 不会误重开浮层。 */
function onComposerKeyUp(e: KeyboardEvent): void {
  if (e.key === 'ArrowLeft' || e.key === 'ArrowRight' || e.key === 'Home' || e.key === 'End') {
    updateMentionState();
  }
}

/** 「点击浮层外部关闭」（document pointerdown：浮层与输入框自身之外一律关）。 */
function onDocPointerDown(e: PointerEvent): void {
  if (!mentionActive.value) return;
  const t = e.target as Node | null;
  if (!t || t === composerInputEl.value || mentionPopupEl.value?.contains(t)) return;
  closeMention();
}

const sortedGroups = computed(() => {
  // 群组/私聊（im_groups）+ 直通消息会话（dm-*，含本地占位）统一排序：
  // 有未读的优先置顶，其次按最近活跃时间
  return [...groups.value, ...dmEntries.value].sort((a, b) => {
    // 有未读的优先置顶
    const ua = unreadOf(a.id);
    const ub = unreadOf(b.id);
    if (ua !== ub) return ub - ua;
    const ta = a.last_activity ? Date.parse(a.last_activity) : 0;
    const tb = b.last_activity ? Date.parse(b.last_activity) : 0;
    return (Number.isNaN(tb) ? 0 : tb) - (Number.isNaN(ta) ? 0 : ta);
  });
});

// =============================================================================
// 生命周期
// =============================================================================
onMounted(() => {
  // 进入页面：有私钥 → 自动挑战-签名认证 → 认证成功才拉数据/连 WS；
  // 无私钥 → 顶部身份卡引导「生成身份 / 导入私钥」。
  // 远程大厅会话项先从 localStorage 恢复；?node= 参数（节点发现页「💬 进入 IM」
  // 跳转）→ 创建/激活对应远程节点大厅会话项。
  loadRemoteLobbies();
  loadCollapseStates(); // 侧栏「节点大厅/节点」分区收起状态（localStorage 记忆）
  handleNodeQuery();
  void bootstrapAuth();
  // @ 浮层「点击外部关闭」（浮层在 composer 内，事件挂 document 才能吃到外部）
  document.addEventListener('pointerdown', onDocPointerDown);
});

onBeforeUnmount(() => {
  teardownChat();
  document.removeEventListener('pointerdown', onDocPointerDown);
});

// 已在 /chat 会话中时 ?node= 变化（如再次从别处跳入）→ 创建/激活远程大厅会话项
watch(
  () => route.query.node,
  () => handleNodeQuery(),
);

watch(currentGroup, async () => {
  await nextTick();
  void scrollToBottom();
});

watch([activeView, activeRemoteNodeId], async () => {
  await nextTick();
  void scrollToBottom();
});
</script>

<template>
  <div class="chat-root">
    <!-- 「蓝牙 mesh 中继」已迁入「网络管理」(/network) 的「BLE Mesh 中继」Tab，本页回归纯 IM -->
    <!-- 聊天视图（原 sidebar + main） -->
    <div class="chat-page">
    <!-- ============ 左：身份卡 + 搜索 + 对话/群组列表 + 节点 ============ -->
    <aside class="chat-sidebar">
      <!-- ============ 顶部身份卡（区块链身份 = secp256k1 公钥）============ -->
      <div v-if="!hasIdentity" class="identity-card">
        <div class="id-head">
          <span class="id-avatar">🔑</span>
          <div class="id-main">
            <span class="id-name">IM 区块链身份</span>
            <span class="id-sub">身份 = secp256k1 公钥，私钥只存本机</span>
          </div>
        </div>
        <div class="id-actions">
          <button class="btn btn-small btn-primary" type="button" @click="startGenerateIdentity">
            生成身份
          </button>
          <button class="btn btn-small" type="button" @click="openImportKey">导入私钥</button>
        </div>
      </div>
      <div v-else class="identity-card ok" :title="pubkey">
        <span class="id-avatar" title="本机身份固定头像（由公钥确定性生成）">
          <img :src="identiconSvg(pubkey, 24)" alt="" />
        </span>
        <div class="id-main">
          <span class="id-name mono">{{ shortPubkey }}</span>
          <span class="id-sub">EVM …{{ displaySuffix }} · {{ authPhaseText }}</span>
        </div>
        <button
          class="id-switch"
          type="button"
          title="清除本机身份（私钥+token），重新生成/导入"
          @click="switchIdentity"
        >
          切换身份
        </button>
      </div>
      <!-- 认证失败：错误 + 重试 -->
      <div v-if="authPhase === 'error'" class="auth-err">
        <span class="auth-err-text">IM 认证失败：{{ authError }}</span>
        <button class="btn btn-small btn-primary" type="button" @click="bootstrapAuth">重试</button>
      </div>

      <!-- 搜索框已移至右侧消息区顶部工具条（随当前会话定搜索范围） -->

      <!-- ============ 固定大厅会话项（列表最顶：我的大厅 / 联邦大厅）============ -->
      <div class="group-list lobby-list">
        <!-- 我的大厅＝本节点公共频道（全员自动加入，可发言） -->
        <button
          type="button"
          class="group-item lobby-item"
          :class="{ active: inMyLobbyView }"
          @click="selectMyLobby"
        >
          <span class="g-avatar lobby-avatar">🏨</span>
          <span class="g-main">
            <span class="g-name">我的大厅</span>
            <span class="g-sub">本节点公共频道 · 连接每一个超级个体</span>
          </span>
          <span
            v-if="myLobbyUnread > 0"
            class="unread-badge"
            title="未读消息"
          >{{ myLobbyUnread }}</span>
          <span
            v-else
            class="online-badge"
            title="在线人数"
          >🟢 {{ lobbyInfo?.online_count ?? 0 }}</span>
        </button>
        <!-- 联邦大厅＝跨节点共享频道（可写：所有连接节点的用户都可在此发言） -->
        <button
          type="button"
          class="group-item lobby-item"
          :class="{ active: inFedLobbyView }"
          title="跨节点共享频道——所有连接的 NexOS 节点的用户都可在此发言（与我的大厅互相隔离）"
          @click="selectFedLobby"
        >
          <span class="g-avatar fed-avatar">🌐</span>
          <span class="g-main">
            <span class="g-name">联邦大厅</span>
            <span class="g-sub">跨节点共享频道 · 可发言</span>
          </span>
          <span v-if="fedLobbyUnread > 0" class="unread-badge" title="未读消息">{{
            fedLobbyUnread
          }}</span>
        </button>
      </div>

      <!-- ============ 对话/群组（布局：我的大厅/联邦大厅 → 对话 → 节点大厅 → 节点）============ -->
      <div class="sidebar-head">
        <h3>对话</h3>
      </div>

      <div v-if="groupsError" class="error-box">群组加载失败：{{ groupsError }}</div>

      <div class="group-list">
        <p v-if="groupsLoading && !groups.length" class="muted hint-pad">加载中…</p>
        <p v-else-if="!groups.length && !groupsError" class="muted hint-pad">
          暂无对话，点击「新建群组」开始。
        </p>
        <button
          v-for="g in sortedGroups"
          :key="g.id"
          type="button"
          class="group-item"
          :class="{ active: currentGroup?.id === g.id }"
          @click="selectGroup(g)"
        >
          <span class="g-avatar" :class="g.kind === 'direct' ? 'avatar-direct' : ''">
            {{ g.id.startsWith('dm-') ? '✉️' : g.kind === 'direct' ? '●' : '#' }}
          </span>
          <span class="g-main">
            <span class="g-name">{{ g.name || g.id }}</span>
            <span class="g-sub">
              <span v-if="g.id.startsWith('dm-')">私信 · 双方可见</span>
              <span v-else-if="g.kind === 'direct'">私聊</span>
              <span v-else>群组</span>
              <span v-if="!g.id.startsWith('dm-') && g.members && g.members.length"> · {{ g.members.length }} 成员</span>
            </span>
          </span>
          <span v-if="unreadOf(g.id) > 0" class="unread-badge">{{ unreadOf(g.id) }}</span>
        </button>
        <!-- 「＋ 新建群组」自区头下置到会话列表末尾（通栏小按钮，空列表时也在） -->
        <button
          type="button"
          class="btn btn-small btn-primary new-group-btn"
          @click="openCreateGroup"
        >＋ 新建群组</button>
      </div>

      <!-- ============ 远程节点大厅会话（?node= 创建，localStorage 持久化；分区可收起）============ -->
      <!-- 标题行常显（空列表也可收起/展开）；v-show 收起只隐藏不销毁，轮询/会话数据保留 -->
      <div
        class="sidebar-divider collapsible"
        role="button"
        tabindex="0"
        :aria-expanded="!collapsedNodeLobbies"
        title="收起/展开节点大厅分区"
        @click="toggleNodeLobbies"
        @keydown.enter.prevent="toggleNodeLobbies"
        @keydown.space.prevent="toggleNodeLobbies"
      >
        <span>节点大厅</span>
        <span class="divider-caret" :class="{ collapsed: collapsedNodeLobbies }" aria-hidden="true">▾</span>
      </div>
      <div v-show="!collapsedNodeLobbies" class="group-list">
        <p v-if="!remoteLobbies.length" class="muted hint-pad">
          暂无远程大厅会话（节点发现页「💬 进入 IM」创建）。
        </p>
        <div
          v-for="r in remoteLobbies"
          :key="r.node_id"
          class="remote-lobby-row"
        >
          <button
            type="button"
            class="group-item remote-lobby-item"
            :class="{ active: inRemoteLobbyView && activeRemoteNodeId === r.node_id }"
            :title="`${remoteLabelOf(r.node_id)} 的大厅（远程 · 经加密 P2P 通道）`"
            @click="selectRemoteLobby(r.node_id)"
          >
            <span class="g-avatar remote-avatar">🏨</span>
            <span class="g-main">
              <span class="g-name">{{ remoteLabelOf(r.node_id) }} 的大厅</span>
              <span class="g-sub">远程节点 · 经 P2P</span>
            </span>
            <span
              v-if="remoteStates[r.node_id]?.phase === 'open'"
              class="online-badge"
              title="对方大厅开放，可浏览/发言"
            >开放</span>
            <span
              v-else-if="remoteStates[r.node_id]?.phase === 'denied'"
              class="remote-badge muted-badge"
              title="对方未开放 IM 大厅"
            >未开放</span>
          </button>
          <button
            type="button"
            class="remote-lobby-remove"
            title="移除该节点大厅会话"
            @click.stop="removeRemoteLobby(r.node_id)"
          >×</button>
        </div>
      </div>

      <!-- 标题行可点击收起/展开（localStorage 记忆）；v-show 收起只隐藏不销毁，30s 轮询照常 -->
      <div
        class="sidebar-divider collapsible"
        role="button"
        tabindex="0"
        :aria-expanded="!collapsedPeers"
        title="收起/展开节点分区"
        @click="togglePeers"
        @keydown.enter.prevent="togglePeers"
        @keydown.space.prevent="togglePeers"
      >
        <span>节点</span>
        <span class="divider-caret" :class="{ collapsed: collapsedPeers }" aria-hidden="true">▾</span>
      </div>

      <!-- P2P 元数据自动发现（30s 轮询）：仅 Active 节点；点击发起对方大厅会话 -->
      <div v-show="!collapsedPeers" class="group-list">
        <p v-if="!activeImNodes.length" class="muted hint-pad">
          暂无活跃节点（经 P2P 自动发现，30s 刷新）。
        </p>
        <button
          v-for="n in activeImNodes"
          :key="n.id"
          type="button"
          class="group-item node-item"
          :class="{ active: inRemoteLobbyView && activeRemoteNodeId === n.id }"
          :title="`进入 ${shortIdentity(n.id)} 的大厅（活跃 · 健康分 ${nodeScore(n)}）`"
          @click="openNodeLobby(n)"
        >
          <span class="g-avatar">🟢</span>
          <span class="g-main">
            <span class="g-name mono">{{ shortIdentity(n.id) }}</span>
            <span class="g-sub">最近在线 {{ relTime(n.last_seen) }}</span>
          </span>
          <span :class="scoreClass(nodeScore(n))">{{ nodeScore(n) }}</span>
        </button>
      </div>
    </aside>

    <!-- ============ 右：消息区（随左侧选中会话切换）============ -->
    <section class="chat-main">
      <!-- ============ 顶栏：会话标题（只显示当前选中会话名）+ 搜索 + ⚙️ 设置（最右）============ -->
      <header class="main-head" :class="{ 'fed-off': inLobbyView && fedReceiveEnabled === false }">
        <div class="main-title">
          <!-- 我的大厅：本节点公共频道 -->
          <template v-if="inMyLobbyView">
            <span class="g-avatar small lobby-avatar">🏨</span>
            <div>
              <div class="t-name">我的大厅</div>
              <div class="t-sub">
                本节点公共频道 · {{ lobbyInfo?.member_count ?? 0 }} 成员 ·
                {{ lobbyInfo?.online_count ?? 0 }} 在线
              </div>
            </div>
          </template>
          <!-- 联邦大厅：跨节点共享频道（可写） -->
          <template v-else-if="inFedLobbyView">
            <span class="g-avatar small fed-avatar">🌐</span>
            <div>
              <div class="t-name">联邦大厅</div>
              <div class="t-sub">
                跨节点共享频道 · 可发言 · {{ fedLobbyMessages.length }} 条 · 与我的大厅互相隔离
              </div>
            </div>
          </template>
          <!-- 远程节点大厅：经 P2P 的对方大厅镜像 -->
          <template v-else-if="inRemoteLobbyView">
            <span class="g-avatar small remote-avatar">🏨</span>
            <div>
              <div class="t-name">{{ remoteLabelOf(activeRemoteNodeId) }} 的大厅</div>
              <div class="t-sub">
                远程节点大厅 · 经加密 P2P 通道 · {{ activeRemoteState.messages.length }} 条
              </div>
            </div>
          </template>
          <!-- 群组/私聊 -->
          <template v-else-if="currentGroup">
            <span class="g-avatar small" :class="currentGroup.kind === 'direct' ? 'avatar-direct' : ''">
              {{ currentGroup.id.startsWith('dm-') ? '✉️' : currentGroup.kind === 'direct' ? '●' : '#' }}
            </span>
            <div>
              <div class="t-name">{{ currentGroup.name || currentGroup.id }}</div>
              <div class="t-sub">
                <span v-if="currentGroup.id.startsWith('dm-')">直通消息 · 只有双方可见</span>
                <span v-else-if="currentGroup.kind === 'direct'">私聊</span>
                <span v-else>群组对话</span>
                <span v-if="!currentGroup.id.startsWith('dm-') && currentGroup.members && currentGroup.members.length">
                  · {{ currentGroup.members.length }} 成员
                </span>
              </div>
            </div>
          </template>
        </div>
        <!-- 我的大厅在线成员头像横条（仅本节点大厅有在线列表；点击头像发私信） -->
        <div v-if="inMyLobbyView" class="lobby-presence">
          <button
            v-for="m in lobbyOnlineMembers"
            :key="m.user_id"
            type="button"
            class="presence-avatar presence-clickable"
            :title="`${lobbyMemberTitle(m)}（点击发私信）`"
            @click="openDm(m.user_id, m.display_name || '')"
          >
            <img :src="lobbyMemberIdenticon(m)" alt="" />
          </button>
          <span v-if="!lobbyOnlineMembers.length" class="presence-empty">暂无在线成员</span>
        </div>
        <!-- 顶栏右侧工具组（margin-left:auto 整体靠右；⚙️ 设置恒为最右按钮）：
             远程大厅=刷新（镜像不支持搜索）；其余会话=搜索框 -->
        <div class="head-tools">
          <button
            v-if="inRemoteLobbyView"
            class="btn btn-small"
            type="button"
            :disabled="activeRemoteState.loading"
            title="刷新对方大厅镜像"
            @click="loadRemoteLobby(activeRemoteNodeId)"
          >
            ↻ 刷新
          </button>
          <div v-if="!inRemoteLobbyView" class="head-search">
            <input
              v-model="searchQuery"
              class="search-input"
              type="text"
              :placeholder="inGroupView ? '搜索本会话消息，回车执行…' : '搜索大厅消息，回车执行…'"
              @keydown.enter="onSearchEnter"
            />
            <button
              v-if="searchQuery || searchMode"
              class="search-clear"
              type="button"
              title="清空搜索，返回消息流"
              @click="clearSearch"
            >×</button>
          </div>
          <!-- IM 设置（⚙️）：联邦接收 / 大厅开放开关收进此面板；按钮固定顶栏最右 -->
          <button
            class="btn btn-small im-settings-btn"
            type="button"
            title="IM 设置（联邦接收 / 允许浏览本机大厅）"
            @click="openImSettings"
          >⚙️ 设置</button>
        </div>
      </header>

      <!-- 联邦接收暂停横幅（大厅类会话；工具条变灰 + 文案提示；恢复入口=右上 ⚙️ 设置） -->
      <div v-if="inLobbyView && (fedReceiveEnabled === false || fedError)" class="fed-paused-banner">
        <span v-if="fedReceiveEnabled === false"
          >🌐 联邦接收已暂停——不再接收其他节点发到联邦大厅的消息（本地消息与发送不受影响），可在右上 ⚙️ 设置 恢复</span
        >
        <span v-if="fedError" class="fed-err">开关操作失败：{{ fedError }}</span>
      </div>

      <div v-if="inMyLobbyView && lobbyError" class="error-box thin">大厅加载失败：{{ lobbyError }}</div>
      <div v-if="inFedLobbyView && fedLobbyError" class="error-box thin">联邦大厅加载失败：{{ fedLobbyError }}</div>
      <div v-if="inGroupView && messagesError" class="error-box thin">消息加载失败：{{ messagesError }}</div>

      <!-- 搜索结果面板（替换消息列表；点「清空返回」回正常消息流） -->
      <div v-if="searchMode" ref="messageListEl" class="message-list search-results">
        <div class="sr-head">
          <span class="sr-title">
            {{ searching ? '搜索中…' : `${searchResults.length} 条结果` }}
            <span v-if="!searching" class="sr-scope">· {{ searchScopeName }} ·「{{ searchQuery }}」</span>
          </span>
          <button class="btn btn-small" type="button" @click="clearSearch">× 清空返回</button>
        </div>
        <p v-if="!searching && !searchResults.length" class="muted hint-pad">无匹配消息。</p>
        <button
          v-for="m in searchResults"
          :key="m.id"
          type="button"
          class="search-item"
          title="点击跳转到该消息所在会话"
          @click="jumpToMessage(m)"
        >
          <span class="si-sender">{{ m.sender_name || m.sender_id }}</span>
          <span class="si-content">{{ m.content }}</span>
          <span class="si-time">{{ fmtTime(m.created_at) }}</span>
        </button>
      </div>

      <!-- 群组视图但无选中会话（服务端会话被删等边缘态） -->
      <div v-else-if="inGroupView && !currentGroup" class="empty-main">
        <div class="empty-card">
          <span class="empty-icon">💬</span>
          <p>选择左侧的一个对话开始聊天，或「新建群组」。</p>
        </div>
      </div>

      <div v-else ref="messageListEl" class="message-list">
        <!-- ============ 我的大厅 / 联邦大厅（同一气泡模板，数据按会话分流）============ -->
        <template v-if="inLobbyView">
          <p v-if="!hasIdentity" class="muted hint-pad">
            还没有 IM 身份——点击左上角「生成身份」或「导入私钥」，认证后自动加入大厅。
          </p>
          <p v-else-if="authPhase === 'error'" class="muted hint-pad">
            IM 认证失败（{{ authError }}），请点击左侧「重试」。
          </p>
          <p v-else-if="!activeLobbyFeedMsgs.length" class="muted hint-pad">
            {{
              inFedLobbyView
                ? '暂无联邦消息——所有连接节点的人都可以在这里发言'
                : '还没有大厅消息，发一条和大家打招呼吧。'
            }}
          </p>
          <template v-for="m in activeLobbyFeedMsgs" :key="m.id">
            <!-- 系统消息（欢迎/广播）：居中灰色 -->
            <div v-if="isSystemMsg(m)" class="sys-msg">
              <span>{{ m.content }}</span>
            </div>
            <!-- 用户消息：展示发送者头像 + 名字（agent 消息加 🤖徽章 + 紫描边；
                 联邦远程消息 sender_id 以 fed: 开头 → 🌐徽章 + 来源节点） -->
            <div v-else class="msg-row" :class="{ own: isOwnMsg(m) }">
              <div class="msg-avatar" :class="{ own: isOwnMsg(m) }" :title="m.sender_name || m.sender_id">
                <img :src="msgIdenticon(m)" alt="" />
              </div>
              <div class="msg-bubble" :class="{ agent: isAgentMsg(m) }">
                <div v-if="!isOwnMsg(m)" class="msg-sender" :title="m.sender_name || m.sender_id">
                  {{ senderDisplay(m) }}<span
                    v-if="isAgentMsg(m)"
                    class="agent-badge"
                    title="AI agent 消息"
                    >🤖</span
                  ><span
                    v-if="isFedMsg(m)"
                    class="fed-badge"
                    :title="`联邦远程消息：来自 ${fedNodeOf(m)} 节点（经 os-p2p 同步）`"
                    >🌐 来自 {{ fedNodeOf(m) }}</span
                  ><button
                    v-if="dmTargetOf(m)"
                    type="button"
                    class="dm-launch-btn"
                    title="给这位发送者发私信（直通消息：不经大厅广播，只有双方可见；跨节点自动定向）"
                    @click="openDm(dmTargetOf(m), m.sender_id.startsWith('fed:') ? '' : m.sender_name || '')"
                  >✉️ 私信</button
                  ><TipButton
                    target-kind="im_message"
                    :target-ref="m.id"
                    :get-token="tipTokenGetter"
                    size="small"
                  />
                </div>
                <button
                  v-if="m.attachment"
                  type="button"
                  class="msg-attachment"
                  title="点击下载附件"
                  :disabled="downloadingFileId === m.attachment.file_id"
                  @click="openAttachment(m.attachment)"
                >
                  <span class="file-icon">📄</span>
                  <span class="att-name">{{ m.attachment.filename }}</span>
                  <span v-if="fmtSize(m.attachment.size_bytes)" class="att-size">{{
                    fmtSize(m.attachment.size_bytes)
                  }}</span>
                  <span class="att-dl">{{
                    downloadingFileId === m.attachment.file_id ? '下载中…' : '下载'
                  }}</span>
                </button>
                <div class="msg-content">
                  <template v-for="(seg, si) in contentSegments(m)" :key="si">
                    <span v-if="seg.mention" class="mention-tag">{{ seg.text }}</span>
                    <template v-else>{{ seg.text }}</template>
                  </template>
                </div>
                <div class="msg-time">{{ fmtTime(m.created_at) }}</div>
              </div>
            </div>
          </template>
        </template>

        <!-- ============ 远程节点大厅只读镜像（经 P2P 查询对方；脱敏消息无附件内容）============ -->
        <template v-else-if="inRemoteLobbyView">
          <p v-if="!hasIdentity" class="muted hint-pad">
            还没有 IM 身份——点击左上角「生成身份」或「导入私钥」，认证后即可浏览该节点的大厅。
          </p>
          <p v-else-if="authPhase === 'error'" class="muted hint-pad">
            IM 认证失败（{{ authError }}），请点击左侧「重试」。
          </p>
          <p v-else-if="activeRemoteState.phase !== 'open'" class="muted hint-pad">
            {{ remotePhaseNotice }}
          </p>
          <p v-else-if="!activeRemoteState.messages.length" class="muted hint-pad">
            对方大厅还没有消息。
          </p>
          <template v-for="m in activeRemoteState.messages" :key="m.id">
            <!-- 系统消息：居中灰色 -->
            <div v-if="m.msg_type === 'system'" class="sys-msg">
              <span>{{ m.content }}</span>
            </div>
            <!-- 用户消息：fed: 前缀带 🌐 来源徽章；自己经远程通道发的右对齐 -->
            <div v-else class="msg-row" :class="{ own: isOwnRemoteMsg(m) }">
              <div
                class="msg-avatar"
                :class="{ own: isOwnRemoteMsg(m) }"
                :title="m.sender_name || m.sender_id"
              >
                <img :src="identiconSvg(m.sender_id, 30)" alt="" />
              </div>
              <div class="msg-bubble" :class="{ agent: m.sender_kind === 'agent' }">
                <div
                  v-if="!isOwnRemoteMsg(m)"
                  class="msg-sender"
                  :title="m.sender_name || m.sender_id"
                >
                  {{ remoteSenderDisplay(m) }}<span
                    v-if="m.sender_kind === 'agent'"
                    class="agent-badge"
                    title="AI agent 消息"
                    >🤖</span
                  ><span
                    v-if="m.sender_id.startsWith('fed:')"
                    class="fed-badge"
                    title="联邦远程消息（经 os-p2p 同步）"
                    >🌐</span
                  >
                </div>
                <div class="msg-content">{{ m.content }}</div>
                <div class="msg-time">{{ fmtTime(m.created_at) }}</div>
              </div>
            </div>
          </template>
          <p v-if="activeRemoteState.phase === 'open'" class="muted hint-pad remote-mirror-note">
            🌐 这是对方节点的只读镜像（最近 20 条，不含附件）；发言经 P2P
            联邦送达对方大厅，要深度参与请加入该节点的 IM。
          </p>
        </template>

        <!-- ============ 群组/私聊消息流 ============ -->
        <template v-else>
          <p v-if="messagesLoading && !messages.length" class="muted hint-pad">加载中…</p>
          <p v-else-if="!messages.length" class="muted hint-pad">还没有消息，发一条吧。</p>
          <template v-for="m in messages" :key="m.id">
            <!-- 系统消息：居中灰色 -->
            <div v-if="isSystemMsg(m)" class="sys-msg">
              <span>{{ m.content }}</span>
            </div>
            <!-- 普通消息（agent 消息加 🤖徽章 + 紫描边） -->
            <div v-else class="msg-row" :class="{ own: isOwnMsg(m) }">
              <div class="msg-avatar" :class="{ own: isOwnMsg(m) }" :title="m.sender_name || m.sender_id">
                <img :src="msgIdenticon(m)" alt="" />
              </div>
              <div class="msg-bubble" :class="{ agent: isAgentMsg(m) }">
                <div v-if="!isOwnMsg(m)" class="msg-sender" :title="m.sender_name || m.sender_id">
                  {{ senderDisplay(m) }}<span
                    v-if="isAgentMsg(m)"
                    class="agent-badge"
                    title="AI agent 消息"
                    >🤖</span
                  ><TipButton
                    target-kind="im_message"
                    :target-ref="m.id"
                    :get-token="tipTokenGetter"
                    size="small"
                  />
                </div>
                <div v-if="replyPreview(m)" class="msg-reply">↪ {{ replyPreview(m) }}</div>
                <div v-if="m.msg_type === 'file' || m.msg_type === 'image'" class="msg-file">
                  <span class="file-icon">{{ m.msg_type === 'image' ? '🖼' : '📎' }}</span>
                  <a v-if="m.file_url" :href="m.file_url" class="file-link" target="_blank" rel="noopener">
                    {{ m.file_url }}
                  </a>
                </div>
                <button
                  v-if="m.attachment"
                  type="button"
                  class="msg-attachment"
                  title="点击下载附件"
                  :disabled="downloadingFileId === m.attachment.file_id"
                  @click="openAttachment(m.attachment)"
                >
                  <span class="file-icon">📄</span>
                  <span class="att-name">{{ m.attachment.filename }}</span>
                  <span v-if="fmtSize(m.attachment.size_bytes)" class="att-size">{{
                    fmtSize(m.attachment.size_bytes)
                  }}</span>
                  <span class="att-dl">{{
                    downloadingFileId === m.attachment.file_id ? '下载中…' : '下载'
                  }}</span>
                </button>
                <div class="msg-content">
                  <template v-for="(seg, si) in contentSegments(m)" :key="si">
                    <span v-if="seg.mention" class="mention-tag">{{ seg.text }}</span>
                    <template v-else>{{ seg.text }}</template>
                  </template>
                </div>
                <div class="msg-time">{{ fmtTime(m.created_at) }}</div>
              </div>
            </div>
          </template>
        </template>
      </div>

      <!-- ============ 输入区（我的大厅/联邦大厅/远程大厅/会话：联邦大厅与远程
             大厅文本-only——联邦/远程通道不承载附件与 @助手）============ -->
      <footer class="composer">
        <!-- 快捷条：待发附件条 + 上传/错误提示（@NexOS助手 快捷按钮暂时移除，@ 手动提及保留） -->
        <div class="composer-quick">
          <span
            v-if="inRemoteLobbyView && activeRemoteState.phase === 'open'"
            class="attach-hint"
            title="发言经 P2P 联邦送达对方大厅；落地以对方开放/接收开关为准"
          >
            🌐 发言将联邦送达 {{ remoteLabelOf(activeRemoteNodeId) }}（文本，不含附件）
          </span>
          <span
            v-if="inFedLobbyView"
            class="attach-hint"
            title="发言广播到所有已连接的 NexOS 节点的联邦大厅（与我的大厅互相隔离）"
          >
            🌐 发言将广播到所有连接的 NexOS 节点（文本，不含附件）
          </span>
          <span v-if="uploading" class="attach-hint">📎 附件上传中…</span>
          <span
            v-if="pendingAttachment"
            class="attach-chip"
            :title="pendingAttachment.filename"
          >
            <span class="attach-chip-name">📄 {{ pendingAttachment.filename }}</span>
            <span class="attach-chip-size">{{ fmtSize(pendingAttachment.size_bytes) }}</span>
            <button
              class="attach-chip-x"
              type="button"
              title="移除待发附件"
              @click="removePendingAttachment"
              >×</button
            >
          </span>
          <span v-if="attachError" class="attach-error" :title="attachError">{{
            attachError
          }}</span>
        </div>
        <div class="composer-row">
          <!-- @ 成员选择浮层：贴输入框上方（absolute 定位于 composer-row，随行宽不外溢） -->
          <div
            v-if="mentionActive && mentionFiltered.length"
            ref="mentionPopupEl"
            class="mention-popup"
          >
            <button
              v-for="(c, i) in mentionFiltered"
              :key="c.id"
              type="button"
              class="mention-item"
              :class="{ active: i === mentionIndex }"
              :title="`@${c.name}`"
              @mouseenter="mentionIndex = i"
              @mousedown.prevent
              @click="applyMention(i)"
            >
              <span class="mi-avatar"><img :src="identiconSvg(c.id, 26)" alt="" /></span>
              <span class="mi-name">{{ c.label }}</span>
              <span v-if="c.agent" class="mi-badge" title="AI agent">🤖</span>
            </button>
          </div>
          <button
            v-if="!inRemoteLobbyView && !inFedLobbyView && !inDmView"
            class="composer-icon"
            type="button"
            title="发送附件（≤64MiB）"
            :disabled="uploading || authPhase !== 'ok'"
            @click="pickFile"
          >
            📎
          </button>
          <textarea
            ref="composerInputEl"
            v-model="draft"
            class="composer-input"
            rows="1"
            :placeholder="
              inRemoteLobbyView
                ? `在 ${remoteLabelOf(activeRemoteNodeId)} 的大厅发言，Enter 发送（经 P2P 联邦）`
                : inFedLobbyView
                  ? '在联邦大厅发言（广播到所有连接节点），Enter 发送，Shift+Enter 换行'
                  : inMyLobbyView
                    ? '在我的大厅发言，Enter 发送，Shift+Enter 换行'
                    : inDmView
                      ? '发私信（只有对方可见，不经大厅广播），Enter 发送'
                      : '输入消息，Enter 发送，Shift+Enter 换行'
            "
            :disabled="sending || authPhase !== 'ok' || (inRemoteLobbyView && activeRemoteState.phase !== 'open')"
            @keydown="onComposerKeydown"
            @input="updateMentionState"
            @click="updateMentionState"
            @keyup="onComposerKeyUp"
          ></textarea>
          <button
            class="btn btn-primary composer-send"
            :disabled="
              sending ||
              uploading ||
              authPhase !== 'ok' ||
              !canSend ||
              (inRemoteLobbyView && activeRemoteState.phase !== 'open')
            "
            @click="sendMessage"
          >
            {{ sending ? '发送中…' : '发送' }}
          </button>
        </div>
        <input ref="fileInputEl" type="file" hidden @change="onFileChosen" />
      </footer>
    </section>

    <!-- ============ 新建群组对话框 ============ -->
    <div v-if="showCreateGroup" class="modal-backdrop" @click.self="closeCreateGroup">
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="cg-title">
        <div class="modal-head">
          <h3 id="cg-title">新建群组</h3>
          <button class="modal-close" type="button" :disabled="createGroupSubmitting" @click="closeCreateGroup">×</button>
        </div>
        <form class="modal-body" @submit.prevent="submitCreateGroup">
          <div class="field">
            <label for="cg-name">群组名</label>
            <input
              id="cg-name"
              v-model="newGroupName"
              type="text"
              placeholder="例如 dev-team"
              :disabled="createGroupSubmitting"
            />
          </div>
          <p v-if="createGroupMsg" :class="['form-msg', `is-${createGroupMsg.kind}`]">{{ createGroupMsg.text }}</p>
          <div class="form-actions">
            <button type="button" class="btn" :disabled="createGroupSubmitting" @click="closeCreateGroup">取消</button>
            <button type="submit" class="btn btn-primary" :disabled="createGroupSubmitting">提交</button>
          </div>
        </form>
      </div>
    </div>

    <!-- ============ 一次性私钥展示（生成身份后，可跳过抄存）============ -->
    <div v-if="showGenKey" class="modal-backdrop">
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="gk-title">
        <div class="modal-head">
          <h3 id="gk-title">身份已生成 —— 请抄存私钥</h3>
          <button class="modal-close" type="button" @click="closeGenKey">×</button>
        </div>
        <div class="modal-body">
          <p class="form-msg is-info">
            私钥仅此一次完整展示，服务器不存储、无法找回；请抄写或复制保存到安全的地方。
          </p>
          <div class="field">
            <label>私钥（hex，仅存本机 localStorage）</label>
            <div class="key-box mono">{{ generatedPrivkey }}</div>
          </div>
          <div class="field">
            <label>公钥（你的 IM 用户名）</label>
            <div class="key-box mono">{{ generatedPubkey }}</div>
          </div>
          <div class="form-actions">
            <button type="button" class="btn" @click="copyGeneratedPrivkey">
              {{ privkeyCopied ? '已复制 ✓' : '复制私钥' }}
            </button>
            <button type="button" class="btn" @click="closeGenKey">跳过</button>
            <button type="button" class="btn btn-primary" @click="closeGenKey">我已抄存</button>
          </div>
        </div>
      </div>
    </div>

    <!-- ============ 导入私钥对话框 ============ -->
    <div v-if="showImportKey" class="modal-backdrop" @click.self="closeImportKey">
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="ik-title">
        <div class="modal-head">
          <h3 id="ik-title">导入私钥</h3>
          <button class="modal-close" type="button" @click="closeImportKey">×</button>
        </div>
        <form class="modal-body" @submit.prevent="submitImportKey">
          <div class="field">
            <label for="ik-key">私钥（64 位 hex，可带 0x 前缀）</label>
            <input
              id="ik-key"
              v-model="importKeyInput"
              type="text"
              class="mono-input"
              placeholder="例如 0x1a2b…（64 位 hex）"
              autocomplete="off"
              spellcheck="false"
            />
          </div>
          <p v-if="importKeyMsg" :class="['form-msg', `is-${importKeyMsg.kind}`]">
            {{ importKeyMsg.text }}
          </p>
          <div class="form-actions">
            <button type="button" class="btn" @click="closeImportKey">取消</button>
            <button type="submit" class="btn btn-primary">导入并认证</button>
          </div>
        </form>
      </div>
    </div>
    <!-- ============ IM 设置面板（选项卡结构，2026-08-24 用户「做成一个选项卡」：
                    Tab1 设置=联邦接收/允许浏览大厅两开关（即时生效）；
                    Tab2 AI Agent 接入=认证三步→WS→进群发言速查）============ -->
    <div v-if="showImSettings" class="modal-backdrop" @click.self="closeImSettings">
      <div class="modal im-settings-modal" role="dialog" aria-modal="true" aria-labelledby="ims-title">
        <div class="modal-head">
          <h3 id="ims-title">IM 设置</h3>
          <button class="modal-close" type="button" @click="closeImSettings">×</button>
        </div>
        <!-- 选项卡条（CodeHub.vue .tabs 同款：文字 Tab + 底部高亮条） -->
        <nav class="im-settings-tabs" role="tablist" aria-label="IM 设置选项卡">
          <button
            type="button"
            class="im-settings-tab"
            :class="{ active: imSettingsTab === 'settings' }"
            role="tab"
            :aria-selected="imSettingsTab === 'settings'"
            @click="imSettingsTab = 'settings'"
          >设置</button>
          <button
            type="button"
            class="im-settings-tab"
            :class="{ active: imSettingsTab === 'agents' }"
            role="tab"
            :aria-selected="imSettingsTab === 'agents'"
            @click="imSettingsTab = 'agents'"
          >AI Agent 接入</button>
        </nav>
        <div class="modal-body">
          <!-- Tab1「设置」：① ② 两个开关（v-show 切换不丢状态） -->
          <div v-show="imSettingsTab === 'settings'" class="im-settings-panel" role="tabpanel">
          <!-- ① 联邦接收（原工具条 toggle 迁入）：开关即时生效 -->
          <div class="im-setting-row">
            <div class="im-setting-main">
              <div class="im-setting-name">
                🌐 联邦接收
                <span v-if="fedToggleBusy" class="im-setting-busy">切换中…</span>
                <span v-else-if="fedReceiveEnabled === null" class="im-setting-busy">加载中…</span>
              </div>
              <div class="im-setting-desc">
                开启后接收其他 NexOS 节点同步来的大厅消息；关闭后暂停接收，本地消息与发送不受影响。
              </div>
            </div>
            <button
              type="button"
              class="im-switch"
              role="switch"
              :aria-checked="fedReceiveEnabled === true"
              :class="{ on: fedReceiveEnabled === true }"
              :disabled="fedToggleBusy || fedReceiveEnabled === null"
              :title="fedToggleTitle"
              @click="toggleFedReceive"
            >
              <span class="im-switch-knob"></span>
            </button>
          </div>
          <p v-if="fedError" class="form-msg is-err">联邦接收切换失败：{{ fedError }}</p>

          <!-- ② 允许浏览本机大厅（原系统设置「IM 联邦」卡片迁入） -->
          <div class="im-setting-row">
            <div class="im-setting-main">
              <div class="im-setting-name">
                🏛 允许浏览本机大厅
                <span v-if="lobbyAccessBusy" class="im-setting-busy">切换中…</span>
                <span v-else-if="!lobbyAccess && !lobbyAccessError" class="im-setting-busy">加载中…</span>
              </div>
              <div class="im-setting-desc">
                开启后同网络的 NexOS 节点可浏览本机 IM 大厅（只读，最近 20 条，不含附件内容），
                并可经加密 P2P 通道远程发言；默认关闭。
              </div>
            </div>
            <button
              type="button"
              class="im-switch"
              role="switch"
              :aria-checked="lobbyAccess?.lobby_public === true"
              :class="{ on: lobbyAccess?.lobby_public === true }"
              :disabled="lobbyAccessBusy || !lobbyAccess"
              title="允许/禁止其他 NexOS 节点浏览本机 IM 大厅"
              @click="toggleLobbyAccess"
            >
              <span class="im-switch-knob"></span>
            </button>
          </div>
          <p v-if="lobbyAccessError" class="form-msg is-err">
            大厅开放开关读取/切换失败：{{ lobbyAccessError }}
          </p>

          <!-- ③ 允许直通消息（DM：点对点私信通道，2026-08-30） -->
          <div class="im-setting-row">
            <div class="im-setting-main">
              <div class="im-setting-name">
                ✉️ 允许直通消息
                <span v-if="dmAccessBusy" class="im-setting-busy">切换中…</span>
                <span v-else-if="!dmAccess && !dmAccessError" class="im-setting-busy">加载中…</span>
              </div>
              <div class="im-setting-desc">
                开启后其他链上身份可直接向你发私信（不经大厅广播，只有双方可见）；
                关闭后对方发送将被拒绝。当前开发阶段默认允许。
              </div>
            </div>
            <button
              type="button"
              class="im-switch"
              role="switch"
              :aria-checked="dmAccess?.dm_open === true"
              :class="{ on: dmAccess?.dm_open === true }"
              :disabled="dmAccessBusy || !dmAccess"
              title="允许/禁止其他身份向你发直通消息（私信）；自己发出的私信不受影响"
              @click="toggleDmAccess"
            >
              <span class="im-switch-knob"></span>
            </button>
          </div>
          <p v-if="dmAccessError" class="form-msg is-err">
            直通消息开关读取/切换失败：{{ dmAccessError }}
          </p>
          </div><!-- /Tab1 设置 -->

          <!-- Tab2「AI Agent 接入」：原 ③ 接入指示区块整块搬入（长内容——
                  面板区内 overflow-y:auto，复制按钮等交互照常） -->
          <div v-show="imSettingsTab === 'agents'" class="im-settings-panel is-agents" role="tabpanel">
          <!-- AI Agent 接入指示（外部 agent 照抄：认证三步 → WS → 进群发言；
                  契约 = im.rs 实际实现，协议全文 docs/IM_AGENTS_AND_FILES.md §6） -->
          <div class="agent-guide">
            <div class="agent-guide-head">
              <div class="agent-guide-title">🤖 AI Agent 接入指示</div>
              <!-- 一键复制完整说明（2026-08-24 用户「这个选项卡的内容需要可以完整复制」：
                      把整个 Tab 拼成自包含纯文本——发给其他 AI agent 直接照做；
                      零散代码块复制按钮保留不动） -->
              <button
                type="button"
                class="btn btn-small btn-primary agent-copy-all"
                :class="{ copied: copiedAgentGuide }"
                title="把本 Tab 全部接入指示（含当前群组清单）复制为一份自包含纯文本，可直接发给其他 AI agent"
                @click="copyAgentGuide"
              >{{ copiedAgentGuide ? '✓ 已复制' : '📋 复制完整接入说明' }}</button>
            </div>
            <p class="agent-guide-sub">
              外部 agent 以自己的链上身份接入本 IM（与人类同一套 REST/WS 通道，无 agent 专用
              API）：认证三步换 token（24h，过期重跑）→ WS 实时收 → 进群发言。
            </p>

            <!-- 基础信息 -->
            <div class="agent-kv">
              <span class="agent-k">本节点地址</span>
              <code class="agent-v">{{ imAgentBase }}</code>
            </div>
            <div class="agent-kv">
              <span class="agent-k">身份</span>
              <span class="agent-v-text">
                secp256k1 公钥（0x + 66 hex 压缩格式）——与 NexHub / 链上身份同款密钥体系，
                可复用同一把私钥
              </span>
            </div>

            <!-- ① 三步认证 -->
            <div class="agent-step-title">① 三步认证（一次性）</div>
            <div class="agent-code">
              <pre class="agent-pre">{{ agentSnippets.challenge }}</pre>
              <button
                type="button"
                class="btn btn-small agent-copy"
                :class="{ copied: copiedAgentSnippet === 'challenge' }"
                @click="copyAgentSnippet('challenge')"
              >{{ copiedAgentSnippet === 'challenge' ? '✓' : '复制' }}</button>
            </div>
            <p class="agent-note">
              → <code>{"nonce":"&lt;64hex&gt;","expires_in":60}</code>（60s 内单次有效）。
              然后本地签名：对 nonce 做 <code>sign(SHA-256(nonce))</code>，取 65 字节
              <code>r||s||v</code> 的 hex（130 字符，可带 0x 前缀）——JS 用
              <code>@noble/secp256k1</code> / Python 用 <code>eth_keys</code> / Rust 用
              <code>k256</code>。
            </p>
            <div class="agent-code">
              <pre class="agent-pre">{{ agentSnippets.verify }}</pre>
              <button
                type="button"
                class="btn btn-small agent-copy"
                :class="{ copied: copiedAgentSnippet === 'verify' }"
                @click="copyAgentSnippet('verify')"
              >{{ copiedAgentSnippet === 'verify' ? '✓' : '复制' }}</button>
            </div>
            <p class="agent-note">
              → <code>{"token":"&lt;64hex&gt;","expires_in":86400}</code>——之后所有 REST
              请求带 <code>Authorization: Bearer &lt;IM token&gt;</code>。
            </p>

            <!-- ② WS 实时收消息 -->
            <div class="agent-step-title">② WS 实时收消息</div>
            <div class="agent-code">
              <pre class="agent-pre">{{ agentSnippets.ws }}</pre>
              <button
                type="button"
                class="btn btn-small agent-copy"
                :class="{ copied: copiedAgentSnippet === 'ws' }"
                @click="copyAgentSnippet('ws')"
              >{{ copiedAgentSnippet === 'ws' ? '✓' : '复制' }}</button>
            </div>
            <p class="agent-note">
              握手即验（token 有效且与 user 匹配，失败 401）。收到的帧（JSON 文本）：
              <code>{"type":"im_message","conversation_id":"…","message":{…}}</code> 或
              <code>im_lobby_message</code>（大厅）——按 conversation_id 分发追加。
            </p>

            <!-- ③ 进群 + 发言 -->
            <div class="agent-step-title">③ 进群 + 发言（群组须先 join）</div>
            <div class="agent-code">
              <pre class="agent-pre">{{ agentSnippets.join }}</pre>
              <button
                type="button"
                class="btn btn-small agent-copy"
                :class="{ copied: copiedAgentSnippet === 'join' }"
                @click="copyAgentSnippet('join')"
              >{{ copiedAgentSnippet === 'join' ? '✓' : '复制' }}</button>
            </div>
            <div class="agent-code">
              <pre class="agent-pre">{{ agentSnippets.send }}</pre>
              <button
                type="button"
                class="btn btn-small agent-copy"
                :class="{ copied: copiedAgentSnippet === 'send' }"
                @click="copyAgentSnippet('send')"
              >{{ copiedAgentSnippet === 'send' ? '✓' : '复制' }}</button>
            </div>
            <p class="agent-note">
              群组非成员发言/补拉 403（join 一次即永久生效，member = token 反查
              pubkey）；<code>sender_kind:"agent"</code> 让前端渲染 AI 徽标；正文里的
              <code>@名字</code> 由服务端自动解析进 mentions（无需自传）。大厅发言改用
              <code>POST /api/v1/im/lobby/messages</code>（GET /lobby 自动加入）。
            </p>
            <div class="agent-code">
              <pre class="agent-pre">{{ agentSnippets.backfill }}</pre>
              <button
                type="button"
                class="btn btn-small agent-copy"
                :class="{ copied: copiedAgentSnippet === 'backfill' }"
                @click="copyAgentSnippet('backfill')"
              >{{ copiedAgentSnippet === 'backfill' ? '✓' : '复制' }}</button>
            </div>
            <p class="agent-note">
              断线补拉：WS 掉线期间的缺口用 <code>after_id</code>（本地最后一条消息
              id）按 id 去重升序追加；大厅同语义
              <code>GET /api/v1/im/lobby/messages?after_id=</code>。
            </p>

            <!-- ④ 本机群组速查 -->
            <div class="agent-step-title">④ 本机群组速查（点击行复制群组 id）</div>
            <p class="agent-note">
              要进「开发组」发言：复制下表「开发组」对应的群组 id，代入上方 join /
              发言的 <code>&lt;group_id&gt;</code>。
            </p>
            <div v-if="agentGroupList.length" class="agent-groups">
              <button
                v-for="g in agentGroupList"
                :key="g.id"
                type="button"
                class="agent-group-row"
                :class="{ target: isDevGroup(g), copied: copiedGroupId === g.id }"
                :title="`复制群组 id：${g.id}`"
                @click="copyGroupId(g.id)"
              >
                <span class="agent-group-name">
                  {{ g.kind === 'group' ? '👥' : '👤' }} {{ g.name }}
                  <span v-if="isDevGroup(g)" class="agent-group-flag">目标群组</span>
                </span>
                <code class="agent-group-id">{{ g.id }}</code>
                <span class="agent-group-copy">
                  {{ copiedGroupId === g.id ? '✓ 已复制' : '复制 id' }}
                </span>
              </button>
            </div>
            <p v-else class="agent-note">（暂无群组/会话——在侧栏创建群组后会出现在这里）</p>

            <!-- ⑤ 完整文档 -->
            <div class="agent-step-title">⑤ 完整文档</div>
            <p class="agent-note">
              完整接入指南（含演示 agent 脚本参考、附件收发、@ 约定、消息推送通知
              webhook）：<code>docs/IM_AGENTS_AND_FILES.md</code>（重点 §6 / §7）。
            </p>
          </div>
          </div><!-- /Tab2 AI Agent 接入 -->

          <div class="form-actions">
            <button type="button" class="btn" @click="closeImSettings">关闭</button>
          </div>
        </div>
      </div>
    </div>
    </div><!-- /chat-page -->
  </div><!-- /chat-root -->
</template>

<style scoped>
/* —— 页面骨架：左列表 + 右消息区 —— */
.chat-root {
  display: flex;
  flex-direction: column;
  /* 浮窗应用：高度填满 WindowFrame 内容区（窗口体 overflow:auto 是外层滚动
     的元凶——绝不用 100vh 定高，否则输入框被顶出可视区需拉到最底） */
  height: 100%;
  min-height: 0;
  background: var(--bg-app, #FAFAFA);
  overflow: hidden;
}

.chat-page {
  display: grid;
  grid-template-columns: 280px 1fr;
  gap: 0;
  padding: 20px 24px;
  flex: 1;
  min-height: 0;
  overflow: hidden;
}

/* ============ 左侧栏 ============ */
.chat-sidebar {
  display: flex;
  flex-direction: column;
  background: var(--bg-card, #ffffff);
  border-right: 1px solid var(--border-soft, #EDEDED);
  min-height: 0;
  position: relative;
}

/* ============ 顶部身份卡（区块链身份 = secp256k1 公钥）============ */
.identity-card {
  padding: 12px 14px;
  border-bottom: 1px solid var(--border-soft, #EDEDED);
  background: linear-gradient(135deg, rgba(119, 33, 111, 0.06), rgba(233, 84, 32, 0.05));
  display: flex;
  align-items: center;
  gap: 10px;
}
.identity-card .id-head {
  display: flex;
  align-items: center;
  gap: 10px;
  flex: 1;
  min-width: 0;
}
.id-avatar {
  width: 32px;
  height: 32px;
  flex: 0 0 auto;
  border-radius: 50%;
  background: rgba(119, 33, 111, 0.12);
  display: inline-flex;
  align-items: center;
  justify-content: center;
  font-size: 15px;
}
.id-main {
  display: flex;
  flex-direction: column;
  min-width: 0;
  flex: 1;
}
.id-name {
  font-size: 13px;
  font-weight: 600;
  color: var(--text, #2B2B2B);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.id-sub {
  font-size: 11.5px;
  color: var(--text-muted, #5E5C5F);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.identity-card:not(.ok) {
  flex-direction: column;
  align-items: stretch;
  gap: 10px;
}
.identity-card.ok .id-name {
  color: #77216F;
}
.id-actions {
  display: flex;
  gap: 8px;
  justify-content: flex-end;
}
.id-switch {
  flex: 0 0 auto;
  border: 1px solid var(--border-soft, #EDEDED);
  border-radius: var(--radius-sm, 8px);
  background: var(--bg-card, #ffffff);
  color: var(--text-muted, #5E5C5F);
  font-size: 11.5px;
  padding: 3px 8px;
  cursor: pointer;
  font-family: inherit;
}
.id-switch:hover {
  color: #b91c1c;
  border-color: rgba(185, 28, 28, 0.35);
}
.auth-err {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 8px 14px;
  border-bottom: 1px solid rgba(185, 28, 28, 0.2);
  background: #fee2e2;
  color: #b91c1c;
  font-size: 12px;
}
.auth-err-text {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/* 一次性私钥 / 公钥展示块 */
.key-box {
  padding: 10px 12px;
  border: 1px dashed var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px);
  background: var(--bg-app, #FAFAFA);
  font-size: 13px;
  word-break: break-all;
  line-height: 1.5;
  user-select: all;
}
.mono-input {
  font-family: var(--mono, monospace);
}

/* 顶栏右侧工具组（搜索 / 刷新 / ⚙️ 设置）：整体推到最右，⚙️ 恒为最右按钮 */
.head-tools {
  display: flex;
  align-items: center;
  gap: 8px;
  margin-left: auto;
  min-width: 0;
}
/* 顶部工具条搜索框（main-head 右侧；回车触发，范围=当前视图） */
.head-search {
  display: flex;
  align-items: center;
  gap: 4px;
  min-width: 0;
}
.search-input {
  width: 200px;
  padding: 6px 10px;
  border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px);
  font-family: inherit;
  font-size: 13px;
  background: var(--bg-app, #FAFAFA);
  color: var(--text, #2B2B2B);
  outline: none;
}
.search-input:focus {
  border-color: var(--accent, #E95420);
  background: var(--bg-card, #ffffff);
}
.search-clear {
  background: transparent;
  border: none;
  font-size: 18px;
  color: var(--text-muted, #5E5C5F);
  cursor: pointer;
  padding: 0 4px;
}
/* 搜索结果面板（替换消息列表区）：N 条结果 + 逐条可点击跳转 */
.sr-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  padding: 10px 14px;
  border-bottom: 1px solid var(--border-soft, #EDEDED);
  position: sticky;
  top: 0;
  background: var(--bg-card, #ffffff);
}
.sr-title {
  font-size: 13px;
  font-weight: 600;
  color: var(--text, #2B2B2B);
}
.sr-scope {
  font-weight: 400;
  font-size: 12px;
  color: var(--text-muted, #5E5C5F);
}
.search-item {
  display: flex;
  flex-direction: column;
  gap: 2px;
  width: 100%;
  text-align: left;
  padding: 8px 12px;
  border: none;
  background: transparent;
  cursor: pointer;
  border-bottom: 1px solid var(--border-soft, #EDEDED);
}
.search-item:hover {
  background: rgba(0, 0, 0, 0.04);
}
.si-sender {
  font-size: 12px;
  font-weight: 600;
  color: var(--accent, #E95420);
}
.si-content {
  font-size: 13px;
  color: var(--text, #2B2B2B);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.si-time {
  font-size: 11px;
  color: var(--text-muted, #5E5C5F);
}
.sidebar-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 12px 14px;
  border-bottom: 1px solid var(--border-soft, #EDEDED);
}
.sidebar-head h3 {
  font-size: 15px;
  font-weight: 600;
  color: var(--text, #2B2B2B);
  margin: 0;
}
.group-list {
  flex: 1 1 auto;
  overflow-y: auto;
  padding: 6px 8px;
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-height: 0;
}
.group-item {
  display: flex;
  align-items: center;
  gap: 10px;
  width: 100%;
  text-align: left;
  padding: 8px 10px;
  border: none;
  background: transparent;
  border-radius: var(--radius-sm, 8px);
  cursor: pointer;
  font-family: inherit;
  color: var(--text, #2B2B2B);
  transition: background 0.12s ease;
}
.group-item:hover {
  background: rgba(0, 0, 0, 0.04);
}
.group-item.active {
  background: var(--accent-soft, rgba(233, 84, 32, 0.1));
  color: var(--accent, #E95420);
}
.g-avatar {
  width: 30px;
  height: 30px;
  flex: 0 0 auto;
  border-radius: 50%;
  background: var(--accent-soft, rgba(233, 84, 32, 0.1));
  color: var(--accent, #E95420);
  font-weight: 700;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  font-size: 13px;
}
.g-avatar.small {
  width: 34px;
  height: 34px;
  font-size: 14px;
}
.avatar-direct {
  background: rgba(14, 132, 32, 0.14);
  color: #15803d;
}
.g-main {
  display: flex;
  flex-direction: column;
  min-width: 0;
  flex: 1;
}
.g-name {
  font-size: 14px;
  font-weight: 500;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.g-sub {
  font-size: 12px;
  color: var(--text-muted, #5E5C5F);
}
.unread-badge {
  flex: 0 0 auto;
  min-width: 18px;
  height: 18px;
  padding: 0 5px;
  border-radius: 9px;
  background: var(--accent, #E95420);
  color: #ffffff;
  font-size: 11px;
  font-weight: 700;
  display: inline-flex;
  align-items: center;
  justify-content: center;
}

.sidebar-divider {
  padding: 8px 14px 4px;
  font-size: 11px;
  font-weight: 600;
  letter-spacing: 0.06em;
  color: var(--text-muted, #5E5C5F);
  text-transform: uppercase;
  border-top: 1px solid var(--border-soft, #EDEDED);
}
/* 可收起分区标题（节点大厅/节点）：flex 布局放指示符，整行可点（hover/键盘可达） */
.sidebar-divider.collapsible {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  padding-right: 10px;
  cursor: pointer;
  user-select: none;
  transition: background 0.12s ease, color 0.12s ease;
}
.sidebar-divider.collapsible:hover {
  background: rgba(0, 0, 0, 0.04);
  color: var(--text, #2B2B2B);
}
.sidebar-divider.collapsible:focus-visible {
  outline: 2px solid var(--accent, #E95420);
  outline-offset: -2px;
}
/* ▾(展开)/▸(收起) 方向指示：rotate 过渡（收起时逆时针转成右向） */
.divider-caret {
  flex: 0 0 auto;
  font-size: 10px;
  line-height: 1;
  text-transform: none;
  transition: transform 0.15s ease;
}
.divider-caret.collapsed {
  transform: rotate(-90deg);
}
/* 「＋ 新建群组」按钮：自区头下置到对话列表末尾（通栏居中，空列表时也在） */
.new-group-btn {
  width: 100%;
  margin-top: 4px;
}
/* 节点健康分徽章（P2P 元数据 score：≥80 绿 / 50-79 中性 / <50 黄） */
.node-score {
  flex: 0 0 auto;
  min-width: 26px;
  height: 18px;
  padding: 0 6px;
  border-radius: 9px;
  font-size: 11px;
  font-weight: 700;
  display: inline-flex;
  align-items: center;
  justify-content: center;
}
.score-hi {
  background: rgba(14, 132, 32, 0.12);
  color: var(--ok, #0E8420);
}
.score-mid {
  background: rgba(119, 33, 111, 0.12);
  color: #77216F;
}
.score-lo {
  background: rgba(249, 155, 17, 0.15);
  color: var(--warn, #F99B11);
}

/* ============ 右侧消息区 ============ */
.chat-main {
  display: flex;
  flex-direction: column;
  min-width: 0;
  min-height: 0;
  background: var(--bg-app, #FAFAFA);
}
.empty-main {
  flex: 1;
  display: flex;
  align-items: center;
  justify-content: center;
}
.empty-card {
  text-align: center;
  color: var(--text-muted, #5E5C5F);
}
.empty-icon {
  font-size: 44px;
  display: block;
  margin-bottom: 8px;
}
.main-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 12px 20px;
  background: var(--glass-bg, rgba(255, 255, 255, 0.72));
  backdrop-filter: var(--glass-blur, blur(20px));
  -webkit-backdrop-filter: var(--glass-blur, blur(20px));
  border-bottom: 1px solid var(--border-soft, #EDEDED);
}
.main-title {
  display: flex;
  align-items: center;
  gap: 12px;
}
.t-name {
  font-size: 15px;
  font-weight: 600;
  color: var(--text, #2B2B2B);
}
.t-sub {
  font-size: 12px;
  color: var(--text-muted, #5E5C5F);
}

.message-list {
  flex: 1;
  overflow-y: auto;
  padding: 16px 20px;
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.sys-msg {
  align-self: center;
  margin: 8px 0;
  padding: 4px 12px;
  font-size: 12px;
  color: var(--text-muted, #8a8a8a);
  background: rgba(0, 0, 0, 0.05);
  border-radius: 12px;
}
.msg-row {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  margin: 4px 0;
}
.msg-row.own {
  flex-direction: row-reverse;
}
.msg-avatar {
  width: 30px;
  height: 30px;
  flex: 0 0 auto;
  border-radius: 50%;
  background: var(--accent-soft, rgba(233, 84, 32, 0.1));
  color: var(--accent, #E95420);
  font-weight: 700;
  font-size: 13px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  margin-top: 2px;
}
.msg-avatar.own {
  background: var(--accent, #E95420);
  color: #ffffff;
}
/* —— identicon 固定头像（由 sender_id 确定性生成，同身份恒同图）—— */
.msg-avatar img,
.presence-avatar img,
.id-avatar img {
  width: 100%;
  height: 100%;
  border-radius: 50%;
  display: block;
}
.msg-bubble {
  max-width: 70%;
  padding: 7px 12px;
  border-radius: 16px;
  background: var(--bg-card, #ffffff);
  border: 1px solid var(--border-soft, #EDEDED);
  color: var(--text, #2B2B2B);
  box-shadow: 0 1px 2px rgba(0, 0, 0, 0.06);
  word-break: break-word;
}
.msg-row.own .msg-bubble {
  background: var(--accent, #E95420);
  color: #ffffff;
  border-color: var(--accent, #E95420);
  border-bottom-right-radius: 4px;
}
.msg-row:not(.own) .msg-bubble {
  border-bottom-left-radius: 4px;
}
.msg-sender {
  font-size: 12px;
  font-weight: 600;
  color: var(--accent, #E95420);
  margin-bottom: 2px;
}
/* —— agent 消息（sender_kind=agent）：🤖徽章 + 紫描边微区分（“（AI 生成）”后缀由后端带出）—— */
.agent-badge {
  margin-left: 4px;
  font-size: 12px;
}
/* —— 联邦远程消息（sender_id 以 fed: 开头，经 os-p2p 从其他节点同步）：🌐徽章 + 来源节点 —— */
.fed-badge {
  margin-left: 6px;
  font-size: 11px;
  font-weight: 600;
  color: #0e8420;
  background: rgba(14, 132, 32, 0.1);
  border-radius: 8px;
  padding: 1px 6px;
}
/* —— 直通消息入口（2026-08-30）：大厅消息发送者旁「✉️ 私信」小按钮 ——
   与正文弱区分（无边框浅底），hover 提亮；点击 openDm 直接开私聊会话 */
.dm-launch-btn {
  margin-left: 6px;
  font-size: 11px;
  font-weight: 600;
  color: var(--accent, #E95420);
  background: rgba(233, 84, 32, 0.08);
  border: none;
  border-radius: 8px;
  padding: 1px 8px;
  cursor: pointer;
  font-family: inherit;
}
.dm-launch-btn:hover {
  background: rgba(233, 84, 32, 0.2);
}
/* 在线成员头像可点击（发私信）：沿用 .presence-avatar 外观，仅归一 button
   默认样式 + 指针与 hover 描边 */
.presence-clickable {
  cursor: pointer;
  padding: 0;
  font-family: inherit;
}
.presence-clickable:hover {
  outline: 2px solid var(--accent, #E95420);
  outline-offset: 1px;
}
/* —— IM 设置（⚙️ 按钮 + 设置面板，2026-08-23：联邦设置迁入本页）—— */
/* 工具条 ⚙️ 按钮：与其他按钮统一风格，右侧固定不换行 */
.im-settings-btn {
  flex: 0 0 auto;
  white-space: nowrap;
  margin-left: 8px;
}
/* 设置面板：加宽居中弹窗（2026-08-24 收入 AI Agent 接入指示后 400→560px，
   代码块更可读；沿用 modal 惯例，窄屏自适应）。
   选项卡化（同日）：弹窗自身不再整体滚动（overflow:hidden 接管 .modal 的
   overflow:auto）——改为 modal-body 弹性填充，接入 Tab 内容过长时在面板内
   overflow-y:auto，设置 Tab 短内容不受影响 */
.im-settings-modal {
  width: min(560px, 100%);
  max-height: 90vh;
  overflow: hidden;
}
/* 选项卡条：CodeHub.vue .tabs/.tab 同款——文字 Tab + 底部高亮条 */
.im-settings-tabs {
  display: flex;
  gap: 4px;
  padding: 0 20px;
  border-bottom: 1px solid var(--border-soft, #ededed);
}
.im-settings-tab {
  padding: 8px 16px;
  background: transparent;
  border: none;
  border-bottom: 2px solid transparent;
  font-size: 14px;
  font-weight: 500;
  color: var(--text-muted, #5e5c5f);
  cursor: pointer;
  font-family: inherit;
  transition: color 0.15s ease, border-color 0.15s ease;
}
.im-settings-tab:hover {
  color: var(--text, #2b2b2b);
}
.im-settings-tab.active {
  color: var(--accent, #e95420);
  border-bottom-color: var(--accent, #e95420);
}
/* Tab 面板：v-show 切换（不丢状态）；内部沿用 modal-body 原 14px 行距，
   包进面板 div 后由面板自身承担；min-height:0 才能把超高内容约束在面板内滚动 */
.im-settings-modal .modal-body {
  flex: 1;
  min-height: 0;
}
.im-settings-panel {
  display: flex;
  flex-direction: column;
  gap: 14px;
  min-height: 0;
}
/* 接入 Tab 内容长：面板内滚动（复制按钮等交互照常；关闭按钮恒在底部可见） */
.im-settings-panel.is-agents {
  flex: 1;
  overflow-y: auto;
}
.im-setting-row {
  display: flex;
  align-items: flex-start;
  gap: 14px;
}
.im-setting-main {
  flex: 1;
  min-width: 0;
}
.im-setting-name {
  font-size: 14px;
  font-weight: 600;
  color: var(--text, #2B2B2B);
}
.im-setting-busy {
  margin-left: 6px;
  font-size: 12px;
  font-weight: 400;
  color: var(--text-muted, #5E5C5F);
}
.im-setting-desc {
  margin-top: 4px;
  font-size: 12.5px;
  line-height: 1.6;
  color: var(--text-muted, #5E5C5F);
}
/* 开关 pill：开=accent 实心 + 滑块右移，关=灰底；禁用半透明 */
.im-switch {
  flex: 0 0 auto;
  width: 42px;
  height: 24px;
  margin-top: 1px;
  padding: 0;
  border: 1px solid var(--border, #d1d5db);
  border-radius: 999px;
  background: rgba(0, 0, 0, 0.08);
  position: relative;
  cursor: pointer;
  font-family: inherit;
  transition: background 0.16s ease, border-color 0.16s ease;
}
.im-switch-knob {
  position: absolute;
  top: 2px;
  left: 2px;
  width: 18px;
  height: 18px;
  border-radius: 50%;
  background: #ffffff;
  box-shadow: 0 1px 3px rgba(0, 0, 0, 0.3);
  transition: transform 0.16s ease;
}
.im-switch.on {
  background: var(--accent, #E95420);
  border-color: var(--accent, #E95420);
}
.im-switch.on .im-switch-knob {
  transform: translateX(18px);
}
.im-switch:disabled {
  opacity: 0.55;
  cursor: not-allowed;
}

/* —— AI Agent 接入指示（原设置面板 ③ → 现「AI Agent 接入」Tab 内容：
   速查卡片 + 深色代码块 + 右上角复制小按钮，
   CodeHub.vue 接入说明 Tab 同款 ob-code/ob-copy 模式的本页小份）—— */
.agent-guide {
  display: flex;
  flex-direction: column;
  gap: 10px;
}
/* 标题行：标题左 + 「📋 复制完整接入说明」主按钮右（窄屏换行） */
.agent-guide-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 10px;
  flex-wrap: wrap;
}
/* 一键复制按钮（btn-primary 体系；成功 ✓ 绿色反馈 1.5s，同零散按钮模式） */
.agent-copy-all.copied {
  background: #16a34a;
  border-color: #16a34a;
  color: #ffffff;
}
.agent-guide-title {
  font-size: 14px;
  font-weight: 700;
  color: var(--text, #2B2B2B);
}
.agent-guide-sub {
  margin: 0;
  font-size: 12.5px;
  line-height: 1.6;
  color: var(--text-muted, #5E5C5F);
}
.agent-kv {
  display: flex;
  align-items: baseline;
  gap: 10px;
  flex-wrap: wrap;
  font-size: 12.5px;
}
.agent-k {
  flex-shrink: 0;
  min-width: 64px;
  font-weight: 600;
  color: var(--text, #2B2B2B);
}
.agent-v {
  font-family: 'Ubuntu Mono', Consolas, monospace;
  font-size: 12px;
  word-break: break-all;
  padding: 2px 8px;
  border-radius: var(--radius-sm, 6px);
  background: var(--bg-code, #fafafa);
  color: var(--text, #2B2B2B);
}
.agent-v-text {
  flex: 1;
  min-width: 0;
  font-size: 12.5px;
  line-height: 1.6;
  color: var(--text-muted, #5E5C5F);
}
.agent-step-title {
  margin-top: 4px;
  font-size: 13px;
  font-weight: 600;
  color: var(--text, #2B2B2B);
}
/* 代码块：深色底 + 等宽（右侧留白给悬浮复制按钮，长命令自动换行） */
.agent-code {
  position: relative;
}
.agent-pre {
  margin: 0;
  padding: 10px 60px 10px 12px;
  border-radius: var(--radius-sm, 8px);
  background: #26292f;
  color: #e8e4e8;
  font-family: 'Ubuntu Mono', 'Cascadia Code', Consolas, monospace;
  font-size: 12px;
  line-height: 1.55;
  white-space: pre-wrap;
  word-break: break-word;
}
/* 右上角复制小按钮（深色底上的半透明样式；成功 ✓ 绿色反馈） */
.agent-copy {
  position: absolute;
  top: 5px;
  right: 5px;
  padding: 2px 9px;
  font-size: 11px;
  background: rgba(255, 255, 255, 0.1);
  border: 1px solid rgba(255, 255, 255, 0.25);
  color: #e8e4e8;
}
.agent-copy:hover {
  background: rgba(255, 255, 255, 0.2);
}
.agent-copy.copied {
  color: #4ade80;
  border-color: rgba(74, 222, 128, 0.55);
  background: rgba(74, 222, 128, 0.12);
}
.agent-note {
  margin: 0;
  font-size: 12.5px;
  line-height: 1.6;
  color: var(--text-muted, #5E5C5F);
}
.agent-note code {
  font-family: 'Ubuntu Mono', Consolas, monospace;
  font-size: 11.5px;
  padding: 1px 5px;
  border-radius: 4px;
  background: var(--bg-code, #fafafa);
  word-break: break-all;
}
/* 群组速查列表（点击整行复制 id；「开发组」目标行高亮） */
.agent-groups {
  display: flex;
  flex-direction: column;
  gap: 6px;
  max-height: 180px;
  overflow: auto;
}
.agent-group-row {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  padding: 6px 8px;
  border: 1px solid var(--border-soft, #ededed);
  border-radius: var(--radius-sm, 8px);
  background: var(--bg-card, #ffffff);
  cursor: pointer;
  text-align: left;
  font-family: inherit;
}
.agent-group-row:hover {
  border-color: var(--accent, #e95420);
}
.agent-group-row.target {
  border-color: rgba(233, 84, 32, 0.45);
  background: rgba(233, 84, 32, 0.06);
}
.agent-group-name {
  flex: 1;
  min-width: 0;
  font-size: 12.5px;
  font-weight: 600;
  color: var(--text, #2B2B2B);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.agent-group-flag {
  margin-left: 6px;
  font-size: 10.5px;
  font-weight: 600;
  color: var(--accent, #e95420);
  background: rgba(233, 84, 32, 0.12);
  border-radius: 999px;
  padding: 1px 6px;
}
.agent-group-id {
  font-family: 'Ubuntu Mono', Consolas, monospace;
  font-size: 11.5px;
  color: var(--text-muted, #5e5c5f);
  word-break: break-all;
  max-width: 45%;
}
.agent-group-copy {
  flex-shrink: 0;
  font-size: 11px;
  color: var(--text-muted, #5e5c5f);
}
.agent-group-row.copied .agent-group-copy {
  color: #0e8420;
  font-weight: 600;
}
/* 暂停态：大厅顶部工具条整体变灰（远程消息接收已停的视觉提示） */
.main-head.fed-off {
  background: rgba(128, 128, 128, 0.14);
}
.fed-paused-banner {
  display: flex;
  flex-wrap: wrap;
  gap: 4px 12px;
  padding: 6px 20px;
  font-size: 12px;
  color: var(--text-muted, #5E5C5F);
  background: rgba(0, 0, 0, 0.05);
  border-bottom: 1px solid var(--border-soft, #EDEDED);
}
.fed-paused-banner .fed-err {
  color: #a91324;
}
/* —— 大厅类会话项（我的大厅/联邦大厅）：紧凑固定区，不参与剩余空间分配 —— */
.lobby-list {
  flex: 0 0 auto;
  padding: 6px 8px 2px;
}
/* 联邦大厅头像（绿系=跨节点联邦语义） */
.fed-avatar {
  background: rgba(14, 132, 32, 0.12);
  color: #0E8420;
}
/* 远程节点大厅头像（蓝系=远程 P2P 语义） */
.remote-avatar {
  background: rgba(51, 82, 128, 0.12);
  color: #335280;
}
/* 远程节点大厅会话行：会话项 + 悬浮显示的 ✕ 移除按钮 */
.remote-lobby-row {
  position: relative;
  display: flex;
  align-items: center;
}
.remote-lobby-row .remote-lobby-item {
  flex: 1 1 auto;
  min-width: 0;
  padding-right: 4px;
}
.remote-lobby-remove {
  flex: 0 0 auto;
  width: 20px;
  height: 20px;
  margin: 0 4px 0 0;
  border: none;
  border-radius: 50%;
  background: transparent;
  color: var(--text-muted, #5E5C5F);
  font-size: 14px;
  line-height: 1;
  cursor: pointer;
  opacity: 0;
  transition: opacity 0.12s ease;
}
.remote-lobby-row:hover .remote-lobby-remove,
.remote-lobby-remove:focus-visible {
  opacity: 1;
}
.remote-lobby-remove:hover {
  background: rgba(169, 19, 36, 0.12);
  color: #a91324;
}
/* 远程大厅开放状态小徽章（开放=绿 / 未开放=灰） */
.remote-badge {
  flex: 0 0 auto;
  height: 18px;
  padding: 0 8px;
  border-radius: 9px;
  display: inline-flex;
  align-items: center;
  font-size: 11px;
  font-weight: 700;
}
.muted-badge {
  background: rgba(0, 0, 0, 0.06);
  color: var(--text-muted, #5E5C5F);
}
/* 远程镜像底部说明（只读 + 联邦发言提示） */
.remote-mirror-note {
  text-align: center;
  padding: 10px 16px 4px;
  font-size: 12px;
}
.msg-bubble.agent {
  border-color: rgba(119, 33, 111, 0.55);
  box-shadow: 0 0 0 1px rgba(119, 33, 111, 0.15), 0 1px 2px rgba(0, 0, 0, 0.06);
  background: linear-gradient(135deg, rgba(119, 33, 111, 0.05), var(--bg-card, #ffffff) 55%);
}
.msg-row.own .msg-bubble.agent {
  border-color: rgba(119, 33, 111, 0.55);
  background: var(--accent, #E95420);
}
/* —— @提及高亮：mentions 命中段渲染为 accent 色 —— */
.mention-tag {
  color: var(--accent, #E95420);
  font-weight: 600;
  background: var(--accent-soft, rgba(233, 84, 32, 0.12));
  border-radius: 4px;
  padding: 0 2px;
}
.msg-row.own .mention-tag {
  color: #ffffff;
  background: rgba(255, 255, 255, 0.22);
}
/* —— 消息附件卡片（attachment 真值来自服务端落盘记录）：点击解码信封另存 —— */
.msg-attachment {
  display: flex;
  align-items: center;
  gap: 6px;
  width: 100%;
  text-align: left;
  margin-bottom: 4px;
  padding: 6px 8px;
  border: 1px solid var(--border-soft, #EDEDED);
  border-radius: 10px;
  background: rgba(0, 0, 0, 0.04);
  font-family: inherit;
  font-size: 13px;
  color: var(--text, #2B2B2B);
  cursor: pointer;
  min-width: 0;
}
.msg-attachment:hover:not(:disabled) {
  border-color: var(--accent, #E95420);
}
.msg-attachment:disabled {
  opacity: 0.6;
  cursor: default;
}
.att-name {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.att-size {
  flex: 0 0 auto;
  font-size: 12px;
  color: var(--text-muted, #5E5C5F);
}
.att-dl {
  flex: 0 0 auto;
  font-size: 12px;
  color: var(--accent, #E95420);
  font-weight: 600;
}
.msg-row.own .msg-attachment {
  background: rgba(255, 255, 255, 0.16);
  border-color: rgba(255, 255, 255, 0.4);
  color: #ffffff;
}
.msg-row.own .att-size {
  color: rgba(255, 255, 255, 0.85);
}
.msg-row.own .att-dl {
  color: #ffffff;
  text-decoration: underline;
}
.msg-reply {
  font-size: 12px;
  color: var(--text-muted, #6b7280);
  background: rgba(0, 0, 0, 0.05);
  border-left: 2px solid var(--accent, #E95420);
  padding: 2px 6px;
  border-radius: 4px;
  margin-bottom: 4px;
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.msg-row.own .msg-reply {
  background: rgba(255, 255, 255, 0.2);
  color: rgba(255, 255, 255, 0.9);
}
.msg-file {
  display: flex;
  align-items: center;
  gap: 6px;
  font-size: 13px;
  margin-bottom: 4px;
}
.file-icon {
  font-size: 16px;
}
.file-link {
  color: var(--accent, #E95420);
  text-decoration: underline;
  word-break: break-all;
}
.msg-row.own .file-link {
  color: #ffffff;
}
.msg-content {
  font-size: 14px;
  line-height: 1.45;
  white-space: pre-wrap;
}
.msg-time {
  font-size: 11px;
  margin-top: 3px;
  opacity: 0.7;
}

.composer {
  flex-shrink: 0; /* 固定于底部：只让 message-list 滚动，输入区绝不被压缩 */
  display: flex;
  flex-direction: column;
  align-items: stretch;
  gap: 6px;
  padding: 10px 20px 12px;
  background: var(--bg-card, #ffffff);
  border-top: 1px solid var(--border-soft, #EDEDED);
}
/* 快捷条：待发附件条 + 上传/错误提示（输入框上方） */
.composer-quick {
  display: flex;
  align-items: center;
  gap: 8px;
  min-height: 24px;
  flex-wrap: wrap;
}
.attach-hint {
  font-size: 12px;
  color: var(--text-muted, #5E5C5F);
  white-space: nowrap;
}
.attach-chip {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  max-width: 60%;
  padding: 2px 4px 2px 10px;
  border-radius: 999px;
  background: rgba(0, 0, 0, 0.05);
  font-size: 12px;
  min-width: 0;
}
.attach-chip-name {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.attach-chip-size {
  color: var(--text-muted, #5E5C5F);
  flex: 0 0 auto;
}
.attach-chip-x {
  border: none;
  background: transparent;
  color: var(--text-muted, #5E5C5F);
  font-size: 15px;
  line-height: 1;
  cursor: pointer;
  padding: 0 5px;
  font-family: inherit;
}
.attach-chip-x:hover {
  color: #b91c1c;
}
.attach-error {
  font-size: 12px;
  color: #b91c1c;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.composer-row {
  display: flex;
  align-items: flex-end;
  gap: 10px;
  /* @ 成员选择浮层的定位锚（浮层贴本行输入框上方，随行宽定宽不外溢） */
  position: relative;
}
.composer-icon {
  width: 40px;
  height: 40px;
  flex: 0 0 auto;
  border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-md, 12px);
  background: var(--bg-app, #FAFAFA);
  font-size: 17px;
  cursor: pointer;
  font-family: inherit;
  line-height: 1;
}
.composer-icon:hover:not(:disabled) {
  border-color: var(--accent, #E95420);
  background: var(--accent-soft, rgba(233, 84, 32, 0.1));
}
.composer-icon:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
.composer-input {
  flex: 1;
  resize: none;
  min-height: 40px;
  max-height: 140px;
  padding: 9px 12px;
  border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-md, 12px);
  font-family: inherit;
  font-size: 14px;
  color: var(--text, #2B2B2B);
  background: var(--bg-app, #FAFAFA);
  outline: none;
}
.composer-input:focus {
  border-color: var(--accent, #E95420);
  box-shadow: 0 0 0 3px rgba(233, 84, 32, 0.15);
  background: var(--bg-card, #ffffff);
}
.composer-send {
  height: 40px;
  padding: 0 18px;
}

/* —— @ 成员选择浮层：贴输入框上方（bottom 锚定 composer-row），左右随行宽；
      窄屏（≤720px 堆叠布局）同样收在行内不外溢，最多 8 项超高内部滚动 —— */
.mention-popup {
  position: absolute;
  bottom: calc(100% + 6px);
  left: 0;
  right: 0;
  max-height: 300px; /* ≈8 项 × 37px */
  overflow-y: auto;
  display: flex;
  flex-direction: column;
  gap: 2px;
  padding: 4px;
  background: var(--bg-card, #ffffff);
  border: 1px solid var(--border-soft, #EDEDED);
  border-radius: var(--radius-md, 12px);
  box-shadow: 0 8px 24px rgba(0, 0, 0, 0.14);
  z-index: 30;
}
.mention-item {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 5px 8px;
  border: none;
  border-radius: var(--radius-sm, 8px);
  background: transparent;
  cursor: pointer;
  font-family: inherit;
  text-align: left;
  min-width: 0;
}
.mention-item.active,
.mention-item:hover {
  background: var(--accent-soft, rgba(233, 84, 32, 0.1));
}
.mi-avatar {
  width: 26px;
  height: 26px;
  flex: 0 0 auto;
  border-radius: 50%;
  background: var(--accent-soft, rgba(233, 84, 32, 0.1));
  display: inline-flex;
  align-items: center;
  justify-content: center;
}
.mi-avatar img {
  width: 100%;
  height: 100%;
  border-radius: 50%;
  display: block;
}
.mi-name {
  flex: 1;
  min-width: 0;
  font-size: 13px;
  color: var(--text, #2B2B2B);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.mi-badge {
  flex: 0 0 auto;
  font-size: 12px;
}

/* ============ 公共：按钮 / 错误 / 模态框 ============ */
.btn {
  padding: 6px 14px;
  border-radius: var(--radius-sm, 8px);
  border: 1px solid var(--border, #d1d5db);
  background: var(--bg-card, #ffffff);
  color: var(--text, #2B2B2B);
  font-size: 13px;
  cursor: pointer;
  font-family: inherit;
  transition: background 0.15s ease, opacity 0.15s ease;
}
.btn:hover {
  background: rgba(0, 0, 0, 0.04);
}
.btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
.btn-small {
  padding: 4px 10px;
  font-size: 12.5px;
}
.btn-primary {
  background: var(--accent, #E95420);
  color: #ffffff;
  border-color: var(--accent, #E95420);
}
.btn-primary:hover:not(:disabled) {
  background: var(--accent-hi, #0077ed);
}

.error-box {
  margin: 10px 14px;
  color: #b91c1c;
  background: #fee2e2;
  border: 1px solid rgba(185, 28, 28, 0.2);
  padding: 8px 12px;
  border-radius: var(--radius-sm, 8px);
  font-size: 13px;
}
.error-box.thin {
  margin: 10px 20px;
}
.muted {
  color: var(--text-muted, #5E5C5F);
  font-size: 13px;
}
.hint-pad {
  padding: 10px 12px;
}
.mono {
  font-family: var(--mono, monospace);
}

.modal-backdrop {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.35);
  backdrop-filter: blur(2px);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 100;
  padding: 16px;
}
.modal {
  width: min(460px, 100%);
  max-height: 90vh;
  overflow: auto;
  background: var(--bg-card, #ffffff);
  border-radius: var(--radius, 16px);
  box-shadow: var(--shadow-modal, 0 20px 60px rgba(0, 0, 0, 0.25));
  display: flex;
  flex-direction: column;
}
.modal-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 16px 20px;
  border-bottom: 1px solid var(--border-soft, #EDEDED);
}
.modal-head h3 {
  font-size: 16px;
  font-weight: 600;
  color: var(--text, #2B2B2B);
  margin: 0;
}
.modal-close {
  background: transparent;
  border: none;
  font-size: 24px;
  line-height: 1;
  color: var(--text-muted, #5E5C5F);
  cursor: pointer;
  padding: 0 6px;
}
.modal-close:hover:not(:disabled) {
  color: var(--text, #2B2B2B);
}
.modal-body {
  padding: 18px 20px;
  display: flex;
  flex-direction: column;
  gap: 14px;
}
.field {
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.field label {
  font-size: 13px;
  color: var(--text, #2B2B2B);
  font-weight: 500;
}
.field input {
  width: 100%;
  padding: 7px 10px;
  border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px);
  font-family: inherit;
  font-size: 14px;
  color: var(--text, #2B2B2B);
  background: var(--bg-card, #ffffff);
}
.field input:focus {
  outline: none;
  border-color: var(--accent, #E95420);
  box-shadow: 0 0 0 3px rgba(233, 84, 32, 0.15);
}
.form-msg {
  font-size: 13px;
  padding: 6px 0;
}
.form-msg.is-err {
  color: #b91c1c;
}
.form-msg.is-ok {
  color: #15803d;
}
.form-msg.is-info {
  color: var(--text-muted, #6b7280);
}
.form-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  margin-top: 4px;
}

/* ============ 大厅（Lobby）Yaru 紫/绿配色 ============ */
.lobby-avatar {
  background: rgba(119, 33, 111, 0.12);
  color: #77216F;
}
.online-badge {
  flex: 0 0 auto;
  min-width: 18px;
  height: 18px;
  padding: 0 8px;
  border-radius: 9px;
  background: rgba(14, 132, 32, 0.12);
  color: #0E8420;
  font-size: 11px;
  font-weight: 700;
  display: inline-flex;
  align-items: center;
  justify-content: center;
}
.lobby-presence {
  display: flex;
  align-items: center;
  gap: 6px;
  overflow: hidden;
}
.presence-avatar {
  width: 28px;
  height: 28px;
  border-radius: 50%;
  background: rgba(14, 132, 32, 0.14);
  color: #0E8420;
  border: 1.5px solid rgba(14, 132, 32, 0.5);
  font-size: 12px;
  font-weight: 700;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  flex: 0 0 auto;
}
.presence-empty {
  font-size: 12px;
  color: var(--text-muted, #5E5C5F);
}

/* —— 响应式：窄屏堆叠 —— */
@media (max-width: 720px) {
  .chat-page {
    grid-template-columns: 1fr;
    grid-template-rows: 220px 1fr;
  }
  .chat-sidebar {
    border-right: none;
    border-bottom: 1px solid var(--border-soft, #EDEDED);
  }
  .msg-bubble {
    max-width: 85%;
  }
}
</style>
