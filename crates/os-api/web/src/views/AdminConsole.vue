<script setup lang="ts">
// =============================================================================
// AdminConsole.vue —— 管理（Web 终端：本地 shell + SSH 远程终端）
//
// 用户定调（原话拆解）：「增加管理功能，与设置功能不冲突，管理功能得有 ssh
// 终端，可以在打开终端」——独立「管理」应用（不并入设置）；后续追加「终端
// 管理加入常用快捷命令」。
//
// 架构（docs/ADMIN_CONSOLE.md）：
//   浏览器 xterm.js ↔ WS（JSON 帧）↔ os-api terminal 组件 ↔ PTY ↔ bash / ssh -tt
//
// 布局：左侧会话栏（+新建：本地终端 / SSH 目标下拉（provisioning targets 只读
// 复用）/ 快速连接表单）+ 右侧终端区（节点状态条 + 多会话 Tab 切换 + 可折叠
// 「快捷命令」面板，每会话一个 xterm 实例）。
//
// xterm 集成要点：
//   - @xterm/xterm + @xterm/addon-fit（官方新包名）；CSS 经
//     `import '@xterm/xterm/css/xterm.css'`（Vite 原生支持 npm 包内 CSS 导入）；
//   - attach 自实现（4 行：onData → input 帧；output 帧 → write），无 addon 依赖；
//   - resize 同步：FitAddon.fit() → terminal.resize 后发 resize 帧（cols/rows），
//     ResizeObserver 监听容器尺寸变化 + 会话切换时重新 fit；
//   - 黑色终端主题（--terminal 色板），与全站 Yaru 风格协调；
//   - 关闭 Tab = DELETE /api/v1/terminal/sessions/:id（kill 进程组 + 关 PTY）；
//   - exit 帧（子进程退出）→ 终端打印提示 + 标记退出（Tab 仍可看回滚）。
//
// 快捷命令面板（2026-08-30）：
//   - 预置命令集（系统/网络/NexOS 运维/Docker 四分类 pill）+ 用户自定义命令
//     （localStorage `admin-quick-cmds` 持久化）；搜索框过滤；
//   - 点击 = 向当前激活终端的 WS 发 input 帧（命令 + \n）——只是往 PTY 写字符，
//     无新增执行面（终端本身已是最高权限）；发送后面板自动收起（可勾选常驻）；
//   - 终端工具：字号 A-/A+（localStorage `admin-term-fontsize`，全部会话生效）、
//     一键清屏（terminal.clear()）、复制全部输出（buffer 遍历序列化 + execCommand
//     回退）；
//   - 会话重命名：Tab ✎ 按钮 / 双击 Tab 标签（localStorage `admin-session-names`
//     记 id→名）。
//
// 节点状态条（2026-08-30）：GET /api/v1/terminal/node-snapshot 一次性聚合
// 版本/在线时长/P2P 连接数/磁盘/内存——点击对应项直接往终端发对应快捷命令
// （点磁盘 → df -h，点内存 → free -h，…）。
//
// WS 鉴权：?token=<admin token>（与 REST 的 Authorization: Bearer 同源
// NEXOS_ADMIN_TOKEN，取自 设置 → API 令牌）。未配置 token 时 WS 会被 401 拒。
// =============================================================================
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { Terminal } from '@xterm/xterm';
import { FitAddon } from '@xterm/addon-fit';
import '@xterm/xterm/css/xterm.css';
import {
  endpoints,
  getApiToken,
  type TerminalNodeSnapshot,
  type TerminalSession,
} from '@/api/client';

// =============================================================================
// localStorage 持久化（自定义命令 / 字号 / 会话名 / 面板常驻开关）
// =============================================================================

const LS_CUSTOM_CMDS = 'admin-quick-cmds';
const LS_FONT_SIZE = 'admin-term-fontsize';
const LS_SESSION_NAMES = 'admin-session-names';
const LS_PANEL_PINNED = 'admin-quickpanel-pinned';

/** 读 localStorage JSON（缺失/解析失败回退默认；隐私模式静默降级）。 */
function lsGet<T>(key: string, fallback: T): T {
  try {
    const raw = localStorage.getItem(key);
    return raw === null ? fallback : (JSON.parse(raw) as T);
  } catch {
    return fallback;
  }
}

/** 写 localStorage JSON（失败忽略——持久化是增强非依赖）。 */
function lsSet(key: string, v: unknown): void {
  try {
    localStorage.setItem(key, JSON.stringify(v));
  } catch {
    /* 隐私模式 / 配额满：忽略 */
  }
}

// =============================================================================
// 会话状态模型（Tab ↔ xterm ↔ WS 三件套同生命周期）
// =============================================================================

interface SessionTab {
  session: TerminalSession;
  terminal: Terminal;
  fit: FitAddon;
  ws: WebSocket | null;
  /** 连接状态：connecting / open / closed / exited */
  state: 'connecting' | 'open' | 'closed' | 'exited';
  /** 退出码（exit 帧携带） */
  exitCode: number | null;
  /** 错误提示（WS 握手失败 / 异常断开） */
  error: string;
  /** resize 观察器（容器尺寸变化 → fit） */
  ro: ResizeObserver | null;
  /** resize 发帧节流句柄 */
  resizeTimer: ReturnType<typeof setTimeout> | null;
}

const tabs = ref<SessionTab[]>([]);
const activeId = ref('');

const msg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);
function friendlyError(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** 当前激活会话 Tab（快捷命令/终端工具的作用目标）。 */
function activeTab(): SessionTab | null {
  return tabs.value.find((t) => t.session.session_id === activeId.value) ?? null;
}

// —— 字号（全局持久化；已开会话实时应用，新会话沿用）——

const FONT_MIN = 9;
const FONT_MAX = 22;
const FONT_DEFAULT = 13;

function clampFont(n: number): number {
  if (!Number.isFinite(n)) return FONT_DEFAULT;
  return Math.min(FONT_MAX, Math.max(FONT_MIN, Math.round(n)));
}

const fontSize = ref<number>(clampFont(lsGet<number>(LS_FONT_SIZE, FONT_DEFAULT)));

/** 调整全部终端字号（±1）并持久化；字号变化改变单元格尺寸 → 重新 fit。 */
function setFont(delta: number): void {
  fontSize.value = clampFont(fontSize.value + delta);
  lsSet(LS_FONT_SIZE, fontSize.value);
  for (const t of tabs.value) {
    t.terminal.options.fontSize = fontSize.value;
  }
  const tab = activeTab();
  if (tab) scheduleFit(tab);
}

// =============================================================================
// xterm ↔ WS attach（自实现 4 行逻辑：onData → input 帧；output 帧 → write）
// =============================================================================

/** base64 编码（UTF-8 安全：TextEncoder 先转字节）。 */
function b64(data: string): string {
  const bytes = new TextEncoder().encode(data);
  let bin = '';
  bytes.forEach((b) => {
    bin += String.fromCharCode(b);
  });
  return btoa(bin);
}

/** base64 解码 → Uint8Array → 字符串（PTY 输出按字节流透传）。 */
function unb64(data: string): string {
  const bin = atob(data);
  const bytes = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
  return new TextDecoder().decode(bytes);
}

/** 建 WS 并 attach 到 xterm 实例（input/output/resize/exit 四帧）。 */
function attachWs(tab: SessionTab): void {
  const token = getApiToken();
  const proto = location.protocol === 'https:' ? 'wss' : 'ws';
  // 测试期默认 admin：token 为空则不拼 query（服务端注入默认 admin），非空照带
  const qs = typeof token === 'string' && token.trim()
    ? `?token=${encodeURIComponent(token)}` : '';
  const url = `${proto}://${location.host}/ws/terminal/${encodeURIComponent(
    tab.session.session_id,
  )}${qs}`;
  const ws = new WebSocket(url);
  ws.binaryType = 'arraybuffer';
  tab.ws = ws;
  tab.state = 'connecting';

  ws.onopen = () => {
    tab.state = 'open';
    tab.error = '';
    // 打开后立即同步当前尺寸（会话创建时用了估算值）
    sendResize(tab);
  };
  ws.onmessage = (ev) => {
    let frame: { type?: string; data?: string; code?: number; msg?: string };
    try {
      frame = JSON.parse(String(ev.data));
    } catch {
      return;
    }
    switch (frame.type) {
      case 'output':
        if (frame.data) tab.terminal.write(unb64(frame.data));
        break;
      case 'exit':
        tab.state = 'exited';
        tab.exitCode = frame.code ?? -1;
        tab.terminal.write(
          `\r\n\x1b[90m── 进程已退出（代码 ${tab.exitCode}）── 关闭标签页释放会话\x1b[0m\r\n`,
        );
        ws.close();
        break;
      case 'error':
        if (frame.msg) {
          tab.terminal.write(`\r\n\x1b[31m[终端错误] ${frame.msg}\x1b[0m\r\n`);
        }
        break;
      default:
        break;
    }
  };
  ws.onerror = () => {
    // 握手 401（未配置/填错 admin token）等；具体状态在 onclose 后提示
    tab.error = 'WebSocket 连接失败（检查 设置 → API 令牌 是否填写有效的 admin token）';
  };
  ws.onclose = () => {
    if (tab.state === 'open') {
      tab.state = 'closed';
      tab.terminal.write('\r\n\x1b[90m── 连接已断开（会话仍在服务端保留，可重开标签连接）──\x1b[0m\r\n');
    } else if (tab.state === 'connecting') {
      tab.state = 'closed';
      tab.error = tab.error || '连接建立失败（401？检查 admin token；或会话已被回收）';
    }
  };

  // 输入：xterm onData → input 帧（含控制键/粘贴，全部透传）
  tab.terminal.onData((data) => {
    if (ws.readyState === WebSocket.OPEN) {
      ws.send(JSON.stringify({ type: 'input', data: b64(data) }));
    }
  });
}

/** fit 后发 resize 帧（服务端 PTY winsize 跟随 + SIGWINCH 传播到子进程）。 */
function sendResize(tab: SessionTab): void {
  if (!tab.ws || tab.ws.readyState !== WebSocket.OPEN) return;
  const { cols, rows } = tab.terminal;
  tab.ws.send(JSON.stringify({ type: 'resize', cols, rows }));
}

/** 容器尺寸变化 → fit（节流 100ms）→ resize 帧。 */
function scheduleFit(tab: SessionTab): void {
  if (tab.resizeTimer !== null) return;
  tab.resizeTimer = setTimeout(() => {
    tab.resizeTimer = null;
    try {
      tab.fit.fit();
    } catch {
      /* 元素不可见时 fit 可能抛错，忽略 */
    }
    sendResize(tab);
  }, 100);
}

/** 创建会话 Tab：xterm 实例 + 挂载 + WS attach。 */
async function openTab(session: TerminalSession): Promise<void> {
  const terminal = new Terminal({
    fontSize: fontSize.value,
    fontFamily: "'Ubuntu Mono', 'JetBrains Mono', Menlo, Consolas, monospace",
    cursorBlink: true,
    // 黑色终端主题（与全站深色控制台风格协调）
    theme: {
      background: '#0c0c10',
      foreground: '#e4e4ec',
      cursor: '#e4e4ec',
      selectionBackground: '#3a3a52',
      black: '#0c0c10',
      red: '#e05561',
      green: '#8cc265',
      yellow: '#d18f52',
      blue: '#4aa0f0',
      magenta: '#c162de',
      cyan: '#42b3c2',
      white: '#e4e4ec',
    },
    scrollback: 5000,
    convertEol: false,
  });
  const fit = new FitAddon();
  terminal.loadAddon(fit);

  const tab: SessionTab = {
    session,
    terminal,
    fit,
    ws: null,
    state: 'connecting',
    exitCode: null,
    error: '',
    ro: null,
    resizeTimer: null,
  };
  tabs.value.push(tab);
  activeId.value = session.session_id;
  await nextTick();
  const host = document.getElementById(`term-host-${session.session_id}`);
  if (host) {
    terminal.open(host);
    try {
      fit.fit();
    } catch {
      /* 首帧尺寸异常忽略 */
    }
    tab.ro = new ResizeObserver(() => scheduleFit(tab));
    tab.ro.observe(host);
  }
  attachWs(tab);
  terminal.focus();
}

/** 切换 Tab：重新 fit（隐藏期间容器尺寸可能变化）+ 聚焦。 */
async function switchTab(id: string): Promise<void> {
  activeId.value = id;
  await nextTick();
  const tab = tabs.value.find((t) => t.session.session_id === id);
  if (tab) {
    scheduleFit(tab);
    tab.terminal.focus();
  }
}

/** 关闭 Tab：DELETE 会话（kill 进程组 + 关 PTY）+ 拆 xterm/WS/observer。 */
async function closeTab(id: string): Promise<void> {
  const idx = tabs.value.findIndex((t) => t.session.session_id === id);
  if (idx === -1) return;
  const tab = tabs.value[idx];
  // 后端清理（幂等；exit 已发生时会话可能已被服务端自清理 → 404 忽略）
  try {
    await endpoints.terminalDeleteSession(id);
  } catch {
    /* 会话已退出/不存在：本地照常收尾 */
  }
  tab.ro?.disconnect();
  if (tab.resizeTimer !== null) clearTimeout(tab.resizeTimer);
  tab.ws?.close();
  tab.terminal.dispose();
  tabs.value.splice(idx, 1);
  // 连带清理自定义会话名（防 localStorage 无限增长）
  if (sessionNames.value[id]) {
    const next = { ...sessionNames.value };
    delete next[id];
    sessionNames.value = next;
    lsSet(LS_SESSION_NAMES, next);
  }
  if (activeId.value === id) {
    activeId.value = tabs.value[Math.max(0, idx - 1)]?.session.session_id ?? '';
  }
  void refreshSessions();
}

// =============================================================================
// 会话创建（三种入口）+ 恢复（页面刷新后重连服务端存活会话）
// =============================================================================

const creating = ref(false);

async function createSession(body: {
  kind: 'local' | 'ssh';
  host?: string;
  port?: number;
  user?: string;
  key_path?: string;
  target_id?: string;
}): Promise<void> {
  if (creating.value) return;
  creating.value = true;
  msg.value = null;
  try {
    const dims = { cols: 100, rows: 30 };
    const session = await endpoints.terminalCreateSession({ ...body, ...dims });
    await openTab(session);
    msg.value =
      body.kind === 'local'
        ? { kind: 'ok', text: '本地终端已打开' }
        : { kind: 'ok', text: `SSH 终端已打开（${session.target}）——密码提示可直接在终端里输入` };
  } catch (e) {
    msg.value = { kind: 'err', text: '会话创建失败：' + friendlyError(e) };
  } finally {
    creating.value = false;
  }
}

// —— SSH 目标下拉（provisioning targets 只读复用）——
interface SshTargetOption {
  id: string;
  name: string;
  host: string;
  port: number;
  user: string;
}
const sshTargets = ref<SshTargetOption[]>([]);
const pickedTargetId = ref('');
const targetsLoading = ref(false);

async function loadSshTargets(): Promise<void> {
  targetsLoading.value = true;
  try {
    const raw = await endpoints.provisioningSshTargets();
    const arr = Array.isArray(raw) ? (raw as Record<string, unknown>[]) : [];
    sshTargets.value = arr
      .filter((t) => typeof t.id === 'string')
      .map((t) => ({
        id: String(t.id),
        name: String(t.name ?? t.id),
        host: String(t.host ?? ''),
        port: Number(t.port ?? 22),
        user: String(t.user ?? 'root'),
      }));
    if (sshTargets.value.length > 0 && !pickedTargetId.value) {
      pickedTargetId.value = sshTargets.value[0].id;
    }
  } catch {
    sshTargets.value = [];
  } finally {
    targetsLoading.value = false;
  }
}

// —— 快速连接表单（临时直连，不入注册表）——
const quick = ref({ host: '', port: 22, user: 'root', key_path: '' });

async function quickConnect(): Promise<void> {
  const host = quick.value.host.trim();
  if (!host) {
    msg.value = { kind: 'err', text: '快速连接需填写主机地址' };
    return;
  }
  const key = quick.value.key_path.trim();
  await createSession({
    kind: 'ssh',
    host,
    port: Number(quick.value.port) || 22,
    user: quick.value.user.trim() || 'root',
    ...(key ? { key_path: key } : {}),
  });
}

// —— 恢复：服务端存活的会话（页面刷新 / 重开应用后续连）——
const restored = ref(false);

async function refreshSessions(): Promise<void> {
  try {
    const list = await endpoints.terminalSessions();
    if (restored.value || list.length === 0) return;
    restored.value = true;
    // 恢复全部存活会话（服务端与 WS 解耦，重连即续流）
    for (const s of list) {
      if (!tabs.value.some((t) => t.session.session_id === s.session_id)) {
        await openTab(s);
      }
    }
  } catch {
    /* 恢复失败不阻塞（无 admin token 时列表 401，新建时会给明确提示） */
  }
}

// =============================================================================
// 快捷命令面板（预置分类命令集 + 自定义命令 + 搜索；点击发 input 帧）
// =============================================================================

type QuickCategory = 'system' | 'network' | 'nexos' | 'docker' | 'custom';

interface QuickCommand {
  id: string;
  name: string;
  command: string;
  category: QuickCategory;
}

const CATEGORY_LABELS: Record<QuickCategory, string> = {
  system: '系统',
  network: '网络',
  nexos: 'NexOS 运维',
  docker: 'Docker',
  custom: '自定义',
};

/** 预置命令集（分类 pill；命令缺失时由 shell 自己报错，如非 Docker 环境）。 */
const PRESET_COMMANDS: QuickCommand[] = [
  // —— 系统 ——
  { id: 'sys-htop', name: '进程监控', command: 'htop', category: 'system' },
  { id: 'sys-df', name: '磁盘空间', command: 'df -h', category: 'system' },
  { id: 'sys-free', name: '内存', command: 'free -h', category: 'system' },
  { id: 'sys-uname', name: '内核信息', command: 'uname -a', category: 'system' },
  { id: 'sys-uptime', name: '在线时长', command: 'uptime', category: 'system' },
  { id: 'sys-svc', name: 'os-api 服务', command: 'systemctl status os-api', category: 'system' },
  {
    id: 'sys-log',
    name: 'os-api 日志',
    command: 'journalctl -u os-api -n 50 --no-pager',
    category: 'system',
  },
  // —— 网络 ——
  { id: 'net-ip', name: '网卡地址', command: 'ip a', category: 'network' },
  { id: 'net-ss', name: '监听端口', command: 'ss -tlnp', category: 'network' },
  { id: 'net-ping', name: '外网连通', command: 'ping -c 3 203.0.113.2', category: 'network' },
  {
    id: 'net-health',
    name: 'API 健康检查',
    command: 'curl -s localhost:8558/api/v1/system/healthz',
    category: 'network',
  },
  // —— NexOS 运维 ——
  {
    id: 'nx-update',
    name: '更新状态',
    command: 'curl -s localhost:8558/api/v1/update/status | head -c 200',
    category: 'nexos',
  },
  {
    id: 'nx-peers',
    name: 'P2P 节点',
    command: 'curl -s localhost:8558/api/v1/p2p/peers',
    category: 'nexos',
  },
  { id: 'nx-zpool', name: 'ZFS 存储池', command: 'zpool status', category: 'nexos' },
  { id: 'nx-zfs', name: 'ZFS 数据集', command: 'zfs list', category: 'nexos' },
  { id: 'nx-gpu', name: 'GPU 状态', command: 'nvidia-smi', category: 'nexos' },
  // —— Docker（非容器环境命令报错属预期，静默换下一招）——
  { id: 'dk-ps', name: '容器列表', command: 'docker ps', category: 'docker' },
];

/** 读回自定义命令并净化（旧数据/手改损坏字段过滤，类型不安全内容丢弃）。 */
function loadCustomCmds(): QuickCommand[] {
  const raw = lsGet<unknown[]>(LS_CUSTOM_CMDS, []);
  if (!Array.isArray(raw)) return [];
  return raw.filter(
    (c): c is QuickCommand =>
      !!c &&
      typeof c === 'object' &&
      typeof (c as QuickCommand).id === 'string' &&
      typeof (c as QuickCommand).name === 'string' &&
      typeof (c as QuickCommand).command === 'string' &&
      (c as QuickCommand).name.trim() !== '' &&
      (c as QuickCommand).command.trim() !== '',
  ).map((c) => ({ ...c, category: 'custom' as const }));
}

const panelOpen = ref(true);
const panelPinned = ref(lsGet<boolean>(LS_PANEL_PINNED, false));
watch(panelPinned, (v) => lsSet(LS_PANEL_PINNED, v));

const cmdSearch = ref('');
const activeCategory = ref<QuickCategory | 'all'>('all');
const customCmds = ref<QuickCommand[]>(loadCustomCmds());

/** 分类 pill 列表（「全部」+ 五分类，顺序稳定）。 */
const categoryPills = computed(() => [
  { key: 'all' as const, label: '全部' },
  ...(Object.keys(CATEGORY_LABELS) as QuickCategory[]).map((k) => ({
    key: k,
    label: CATEGORY_LABELS[k],
  })),
]);

const allCommands = computed<QuickCommand[]>(() => [...PRESET_COMMANDS, ...customCmds.value]);

/** 搜索 + 分类双重过滤（名称/命令文本不区分大小写包含匹配）。 */
const visibleCommands = computed<QuickCommand[]>(() => {
  const q = cmdSearch.value.trim().toLowerCase();
  return allCommands.value.filter(
    (c) =>
      (activeCategory.value === 'all' || c.category === activeCategory.value) &&
      (q === '' ||
        c.name.toLowerCase().includes(q) ||
        c.command.toLowerCase().includes(q)),
  );
});

/** 向当前激活终端发送一条命令（input 帧 = 命令 + \n；与手敲完全等价）。 */
function sendCommand(command: string): void {
  const tab = activeTab();
  if (!tab) {
    msg.value = { kind: 'err', text: '先打开一个终端会话，再使用快捷命令' };
    return;
  }
  if (!tab.ws || tab.ws.readyState !== WebSocket.OPEN) {
    msg.value = { kind: 'err', text: '当前会话未连接（等待重连或关闭后重开会话）' };
    return;
  }
  tab.ws.send(JSON.stringify({ type: 'input', data: b64(command + '\n') }));
  tab.terminal.focus();
  // 默认发送后收起面板腾出终端空间；勾选「常驻」保持展开
  if (!panelPinned.value) panelOpen.value = false;
}

// —— 自定义命令（添加/删除，localStorage 持久化）——
const showCustomForm = ref(false);
const newCmd = ref({ name: '', command: '' });

function addCustomCmd(): void {
  const name = newCmd.value.name.trim();
  const command = newCmd.value.command.trim();
  if (!name || !command) {
    msg.value = { kind: 'err', text: '自定义命令需同时填写名称与命令' };
    return;
  }
  if (customCmds.value.some((c) => c.name === name)) {
    msg.value = { kind: 'err', text: `已存在同名命令「${name}」` };
    return;
  }
  customCmds.value.push({
    id: `custom-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
    name,
    command,
    category: 'custom',
  });
  lsSet(LS_CUSTOM_CMDS, customCmds.value);
  newCmd.value = { name: '', command: '' };
  showCustomForm.value = false;
  activeCategory.value = 'custom';
  cmdSearch.value = '';
  msg.value = { kind: 'ok', text: `已添加自定义命令「${name}」` };
}

function removeCustomCmd(id: string): void {
  customCmds.value = customCmds.value.filter((c) => c.id !== id);
  lsSet(LS_CUSTOM_CMDS, customCmds.value);
}

// =============================================================================
// 终端体验工具（清屏 / 复制全部输出）
// =============================================================================

/** 一键清屏：清空当前终端视口（保留提示行为新首行），不清服务端 scrollback。 */
function clearActive(): void {
  const tab = activeTab();
  if (!tab) {
    msg.value = { kind: 'err', text: '先打开一个终端会话' };
    return;
  }
  tab.terminal.clear();
  tab.terminal.focus();
}

/** 复制文本：Clipboard API 优先，非安全上下文/旧浏览器回退 execCommand。 */
async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    try {
      const ta = document.createElement('textarea');
      ta.value = text;
      ta.style.position = 'fixed';
      ta.style.opacity = '0';
      document.body.appendChild(ta);
      ta.select();
      const ok = document.execCommand('copy');
      ta.remove();
      return ok;
    } catch {
      return false;
    }
  }
}

/** 复制当前终端全部输出（含回滚区：xterm buffer 逐行序列化为纯文本）。 */
async function copyOutput(): Promise<void> {
  const tab = activeTab();
  if (!tab) {
    msg.value = { kind: 'err', text: '先打开一个终端会话' };
    return;
  }
  const buf = tab.terminal.buffer.active;
  const lines: string[] = [];
  for (let i = 0; i < buf.length; i++) {
    const line = buf.getLine(i);
    lines.push(line ? line.translateToString(true) : '');
  }
  const text = lines.join('\n');
  if (!text.trim()) {
    msg.value = { kind: 'info', text: '终端暂无可复制的输出' };
    return;
  }
  const ok = await copyText(text);
  msg.value = ok
    ? { kind: 'ok', text: `已复制 ${lines.length} 行终端输出` }
    : { kind: 'err', text: '复制失败（浏览器剪贴板权限被拒）' };
}

// =============================================================================
// 会话重命名（Tab ✎ / 双击标签；localStorage 记 id → 自定义名）
// =============================================================================

const sessionNames = ref<Record<string, string>>(lsGet<Record<string, string>>(LS_SESSION_NAMES, {}));
const renamingId = ref('');
const renamingText = ref('');

function startRename(tab: SessionTab): void {
  renamingId.value = tab.session.session_id;
  renamingText.value = sessionNames.value[tab.session.session_id] ?? tabLabel(tab);
  void nextTick(() => {
    const el = document.getElementById(`rename-${tab.session.session_id}`);
    el?.focus();
    (el as HTMLInputElement | null)?.select();
  });
}

/** 确认重命名（Enter/失焦；空名 = 放弃改名回落默认名）。 */
function commitRename(): void {
  const id = renamingId.value;
  if (!id) return;
  renamingId.value = '';
  const name = renamingText.value.trim();
  if (!name) return;
  const next = { ...sessionNames.value, [id]: name };
  sessionNames.value = next;
  lsSet(LS_SESSION_NAMES, next);
}

function cancelRename(): void {
  renamingId.value = '';
}

// =============================================================================
// 节点状态条（node-snapshot 聚合：版本/在线时长/P2P/磁盘/内存，点击发命令）
// =============================================================================

const snapshot = ref<TerminalNodeSnapshot | null>(null);
const snapshotLoading = ref(false);

async function loadSnapshot(): Promise<void> {
  snapshotLoading.value = true;
  try {
    snapshot.value = await endpoints.terminalNodeSnapshot();
  } catch {
    // 无 admin token / 网关不可达：状态条整体隐藏（侧栏已有令牌提示）
    snapshot.value = null;
  } finally {
    snapshotLoading.value = false;
  }
}

/** 在线时长人性化（天/小时/分钟，两段粒度）。 */
function fmtUptime(secs: number): string {
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  if (d > 0) return `${d} 天 ${h} 小时`;
  if (h > 0) return `${h} 小时 ${m} 分`;
  return `${m} 分钟`;
}

onMounted(() => {
  void loadSshTargets();
  void refreshSessions();
  void loadSnapshot();
});

onBeforeUnmount(() => {
  // 组件卸载：拆 UI 资源；会话本体保留在服务端（空闲 30 分钟自动回收）
  for (const tab of tabs.value) {
    tab.ro?.disconnect();
    if (tab.resizeTimer !== null) clearTimeout(tab.resizeTimer);
    tab.ws?.close();
    tab.terminal.dispose();
  }
});

// —— Tab 展示辅助 ——
function tabLabel(tab: SessionTab): string {
  const custom = sessionNames.value[tab.session.session_id];
  if (custom) return custom;
  return tab.session.kind === 'local' ? '本地' : tab.session.target;
}
function tabStateClass(tab: SessionTab): string {
  switch (tab.state) {
    case 'open':
      return 'st-open';
    case 'connecting':
      return 'st-connecting';
    case 'exited':
      return 'st-exited';
    default:
      return 'st-closed';
  }
}
</script>

<template>
  <div class="admin-page">
    <div class="page-head">
      <div>
        <h2 class="page-title">管理</h2>
        <div class="page-sub muted">Web 终端 · 本地 shell / SSH 远程（密码认证可直接输）</div>
      </div>
      <div class="head-actions">
        <button class="btn btn-small" :disabled="creating" @click="refreshSessions">刷新</button>
      </div>
    </div>

    <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>

    <!-- =================== 节点状态条（node-snapshot 聚合） =================== -->
    <div v-if="snapshot" class="snapshot-bar">
      <span class="snap-label muted">节点概况</span>
      <button
        class="snap-chip"
        title="点击发送：curl -s localhost:8558/api/v1/update/status | head -c 200"
        @click="sendCommand('curl -s localhost:8558/api/v1/update/status | head -c 200')"
      >
        <span class="snap-k">版本</span><span class="snap-v mono">v{{ snapshot.version }}</span>
      </button>
      <button class="snap-chip" title="点击发送：uptime" @click="sendCommand('uptime')">
        <span class="snap-k">在线</span><span class="snap-v mono">{{ fmtUptime(snapshot.uptime_secs) }}</span>
      </button>
      <button
        class="snap-chip"
        title="点击发送：curl -s localhost:8558/api/v1/p2p/peers"
        @click="sendCommand('curl -s localhost:8558/api/v1/p2p/peers')"
      >
        <span class="snap-k">P2P</span>
        <span class="snap-v mono">{{
          snapshot.p2p_connected === null ? '未启用' : `${snapshot.p2p_connected} 节点`
        }}</span>
      </button>
      <button class="snap-chip" title="点击发送：df -h" @click="sendCommand('df -h')">
        <span class="snap-k">磁盘</span>
        <span class="snap-v mono" :class="{ 'snap-warn': snapshot.disk_use_pct >= 85 }"
          >{{ snapshot.disk_use_pct }}%</span
        >
      </button>
      <button class="snap-chip" title="点击发送：free -h" @click="sendCommand('free -h')">
        <span class="snap-k">内存</span>
        <span class="snap-v mono" :class="{ 'snap-warn': snapshot.mem_use_pct >= 90 }"
          >{{ snapshot.mem_use_pct }}%</span
        >
      </button>
      <button
        class="snap-refresh"
        title="刷新节点状态"
        :disabled="snapshotLoading"
        @click="loadSnapshot"
      >
        ↻
      </button>
    </div>

    <div class="console-wrap">
      <!-- =================== 左侧：会话栏 + 新建 =================== -->
      <aside class="side">
        <div class="side-block">
          <button class="btn new-local" :disabled="creating" @click="createSession({ kind: 'local' })">
            ＋ 本地终端
          </button>
        </div>

        <div class="side-block">
          <div class="side-title">SSH 目标</div>
          <select v-model="pickedTargetId" class="input select" :disabled="targetsLoading">
            <option value="" disabled>{{ targetsLoading ? '加载中…' : '选择目标' }}</option>
            <option v-for="t in sshTargets" :key="t.id" :value="t.id">
              {{ t.name }}（{{ t.user }}@{{ t.host }}:{{ t.port }}）
            </option>
          </select>
          <button
            class="btn"
            :disabled="creating || !pickedTargetId"
            @click="createSession({ kind: 'ssh', target_id: pickedTargetId })"
          >
            连接目标
          </button>
          <div v-if="sshTargets.length === 0 && !targetsLoading" class="side-hint muted">
            暂无目标——在「系统自举 → SSH 远程部署」添加后此处可选
          </div>
        </div>

        <div class="side-block">
          <div class="side-title">快速连接</div>
          <input v-model="quick.host" class="input" placeholder="主机 host / IP" spellcheck="false" />
          <div class="quick-row">
            <input v-model="quick.user" class="input" placeholder="用户" spellcheck="false" />
            <input v-model="quick.port" class="input" type="number" min="1" max="65535" placeholder="端口" />
          </div>
          <input
            v-model="quick.key_path"
            class="input mono"
            placeholder="私钥绝对路径（可选，如 /home/oem/.ssh/id_ed25519）"
            spellcheck="false"
          />
          <button class="btn" :disabled="creating" @click="quickConnect">连接</button>
          <div class="side-hint muted">密码认证也可用——提示出现后直接在终端里输入</div>
        </div>

        <div class="side-block grow">
          <div class="side-title">会话上限</div>
          <div class="side-hint muted">
            同时最多 8 个会话（超限 429）；空闲 30 分钟自动回收。全部操作需 admin 令牌
            （设置 → API 令牌）。Tab 双击或 ✎ 可重命名。
          </div>
        </div>
      </aside>

      <!-- =================== 右侧：终端区（多 Tab + 快捷命令面板） =================== -->
      <section class="term-area">
        <div v-if="tabs.length === 0" class="term-empty">
          <div class="term-empty-title">尚无终端会话</div>
          <div class="muted">
            从左侧新建：本地终端 / SSH 目标 / 快速连接。<br />
            浏览器 xterm.js ↔ WebSocket ↔ PTY ↔ bash / ssh -tt
          </div>
        </div>
        <template v-else>
          <div class="term-tabs">
            <div
              v-for="tab in tabs"
              :key="tab.session.session_id"
              :class="['term-tab', { active: tab.session.session_id === activeId }]"
              @click="switchTab(tab.session.session_id)"
              @dblclick="startRename(tab)"
            >
              <span :class="['st-dot', tabStateClass(tab)]" :title="tab.state" />
              <input
                v-if="renamingId === tab.session.session_id"
                :id="`rename-${tab.session.session_id}`"
                v-model="renamingText"
                class="term-tab-rename mono"
                maxlength="40"
                @click.stop
                @keyup.enter="commitRename"
                @keyup.esc="cancelRename"
                @blur="commitRename"
              />
              <template v-else>
                <span class="term-tab-label mono">{{ tabLabel(tab) }}</span>
                <button
                  class="term-tab-icon"
                  title="重命名会话"
                  @click.stop="startRename(tab)"
                >
                  ✎
                </button>
              </template>
              <button
                class="term-tab-close"
                title="关闭会话（kill 进程 + 关 PTY）"
                @click.stop="closeTab(tab.session.session_id)"
              >
                ×
              </button>
            </div>
          </div>

          <!-- ============ 快捷命令面板（可折叠；点击发到当前激活终端） ============ -->
          <div class="quick-cmds" :class="{ collapsed: !panelOpen }">
            <div class="qc-head">
              <button class="qc-toggle" @click="panelOpen = !panelOpen">
                {{ panelOpen ? '▾' : '▸' }} 快捷命令
              </button>
              <input
                v-if="panelOpen"
                v-model="cmdSearch"
                class="qc-search"
                placeholder="搜索命令…"
                spellcheck="false"
              />
              <label v-if="panelOpen" class="qc-pin" title="勾选后发送命令不自动收起面板">
                <input v-model="panelPinned" type="checkbox" /> 常驻
              </label>
              <button
                v-if="panelOpen"
                class="qc-add"
                title="添加自定义快捷命令"
                @click="showCustomForm = !showCustomForm"
              >
                ＋ 自定义
              </button>
              <div class="qc-tools">
                <button class="qc-tool" title="减小字号" @click="setFont(-1)">A-</button>
                <span class="qc-font mono">{{ fontSize }}px</span>
                <button class="qc-tool" title="增大字号" @click="setFont(1)">A+</button>
                <button class="qc-tool" title="清屏（清空当前终端视口）" @click="clearActive">
                  清屏
                </button>
                <button
                  class="qc-tool"
                  title="复制当前终端全部输出（含回滚区）"
                  @click="copyOutput"
                >
                  复制输出
                </button>
              </div>
            </div>
            <div v-if="panelOpen" class="qc-body">
              <div v-if="showCustomForm" class="qc-form">
                <input
                  v-model="newCmd.name"
                  class="input"
                  placeholder="名称（如：重启网络）"
                  maxlength="30"
                  spellcheck="false"
                />
                <input
                  v-model="newCmd.command"
                  class="input mono"
                  placeholder="命令（如：sudo systemctl restart NetworkManager）"
                  spellcheck="false"
                  @keyup.enter="addCustomCmd"
                />
                <button class="btn btn-small" @click="addCustomCmd">添加</button>
                <button class="btn btn-small" @click="showCustomForm = false">取消</button>
              </div>
              <div class="qc-cats">
                <button
                  v-for="p in categoryPills"
                  :key="p.key"
                  :class="['qc-cat', { active: activeCategory === p.key }]"
                  @click="activeCategory = p.key"
                >
                  {{ p.label }}
                </button>
              </div>
              <div class="qc-list">
                <button
                  v-for="c in visibleCommands"
                  :key="c.id"
                  class="qc-cmd"
                  :title="`${c.name} → ${c.command}`"
                  @click="sendCommand(c.command)"
                >
                  <span class="qc-cmd-name">{{ c.name }}</span>
                  <span class="qc-cmd-cat">{{ CATEGORY_LABELS[c.category] }}</span>
                  <span
                    v-if="c.category === 'custom'"
                    class="qc-cmd-del"
                    title="删除该自定义命令"
                    @click.stop="removeCustomCmd(c.id)"
                  >
                    ×
                  </span>
                </button>
                <span v-if="visibleCommands.length === 0" class="qc-empty muted">
                  无匹配命令（清空搜索或切换分类）
                </span>
              </div>
            </div>
          </div>

          <div
            v-for="tab in tabs"
            v-show="tab.session.session_id === activeId"
            :key="tab.session.session_id"
            class="term-host"
          >
            <div :id="`term-host-${tab.session.session_id}`" class="xterm-mount" />
            <div v-if="tab.error" class="term-error">{{ tab.error }}</div>
          </div>
        </template>
      </section>
    </div>
  </div>
</template>

<style scoped>
.admin-page {
  display: flex;
  flex-direction: column;
  gap: 12px;
  height: 100%;
  min-height: 480px;
}
.page-head {
  display: flex;
  justify-content: space-between;
  align-items: flex-start;
  gap: 12px;
}
.page-title {
  margin: 0;
  font-size: 20px;
}
.page-sub {
  font-size: 12px;
}

/* —— 节点状态条（顶部概况；点击项 = 发对应命令） —— */
.snapshot-bar {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
  padding: 8px 12px;
  border: 1px solid rgba(255, 255, 255, 0.08);
  border-radius: 10px;
  background: rgba(255, 255, 255, 0.03);
}
.snap-label {
  font-size: 11px;
  margin-right: 2px;
}
.snap-chip {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 4px 10px;
  border-radius: 999px;
  border: 1px solid rgba(255, 255, 255, 0.12);
  background: rgba(255, 255, 255, 0.05);
  color: inherit;
  font-size: 12px;
  cursor: pointer;
}
.snap-chip:hover {
  background: rgba(122, 162, 247, 0.14);
  border-color: rgba(122, 162, 247, 0.45);
}
.snap-k {
  font-size: 11px;
  opacity: 0.6;
}
.snap-v {
  font-weight: 600;
}
.snap-warn {
  color: #ffb86c;
}
.snap-refresh {
  margin-left: auto;
  border: none;
  background: none;
  color: inherit;
  font-size: 15px;
  cursor: pointer;
  opacity: 0.65;
  padding: 2px 6px;
  border-radius: 6px;
}
.snap-refresh:hover:not(:disabled) {
  opacity: 1;
  background: rgba(255, 255, 255, 0.1);
}
.snap-refresh:disabled {
  opacity: 0.35;
  cursor: not-allowed;
}

/* —— 主体：左栏 + 终端区 —— */
.console-wrap {
  display: flex;
  gap: 12px;
  flex: 1;
  min-height: 0;
}
.side {
  width: 230px;
  flex: none;
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.side-block {
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 12px;
  border: 1px solid rgba(255, 255, 255, 0.08);
  border-radius: 10px;
  background: rgba(255, 255, 255, 0.03);
}
.side-block.grow {
  flex: 1;
}
.side-title {
  font-size: 12px;
  font-weight: 600;
  letter-spacing: 0.04em;
  opacity: 0.8;
}
.side-hint {
  font-size: 11px;
  line-height: 1.5;
}
.quick-row {
  display: flex;
  gap: 6px;
}
.new-local {
  font-weight: 600;
}

/* —— 终端区 —— */
.term-area {
  flex: 1;
  min-width: 0;
  display: flex;
  flex-direction: column;
  border: 1px solid rgba(255, 255, 255, 0.1);
  border-radius: 10px;
  overflow: hidden;
  background: #0c0c10;
}
.term-empty {
  flex: 1;
  display: flex;
  flex-direction: column;
  align-items: center;
  justify-content: center;
  gap: 10px;
  text-align: center;
  padding: 24px;
}
.term-empty-title {
  font-size: 15px;
  font-weight: 600;
}

/* —— Tab 条 —— */
.term-tabs {
  display: flex;
  gap: 4px;
  padding: 6px 8px 0;
  background: #14141a;
  border-bottom: 1px solid rgba(255, 255, 255, 0.08);
  overflow-x: auto;
}
.term-tab {
  display: flex;
  align-items: center;
  gap: 7px;
  padding: 6px 10px;
  border-radius: 8px 8px 0 0;
  font-size: 12px;
  cursor: pointer;
  color: rgba(228, 228, 236, 0.65);
  border: 1px solid transparent;
  border-bottom: none;
  white-space: nowrap;
}
.term-tab.active {
  color: #e4e4ec;
  background: #0c0c10;
  border-color: rgba(255, 255, 255, 0.12);
}
.term-tab-label {
  max-width: 180px;
  overflow: hidden;
  text-overflow: ellipsis;
}
.term-tab-rename {
  width: 130px;
  padding: 2px 6px;
  border-radius: 5px;
  border: 1px solid rgba(122, 162, 247, 0.6);
  background: rgba(0, 0, 0, 0.4);
  color: #e4e4ec;
  font-size: 12px;
}
.term-tab-rename:focus {
  outline: none;
}
.term-tab-icon {
  border: none;
  background: none;
  color: inherit;
  font-size: 11px;
  line-height: 1;
  padding: 0 1px;
  border-radius: 4px;
  cursor: pointer;
  opacity: 0;
}
.term-tab:hover .term-tab-icon {
  opacity: 0.6;
}
.term-tab-icon:hover {
  opacity: 1;
  background: rgba(255, 255, 255, 0.12);
}
.term-tab-close {
  border: none;
  background: none;
  color: inherit;
  font-size: 14px;
  line-height: 1;
  padding: 0 2px;
  border-radius: 4px;
  cursor: pointer;
  opacity: 0.6;
}
.term-tab-close:hover {
  opacity: 1;
  background: rgba(255, 255, 255, 0.12);
}
.st-dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  flex: none;
}
.st-dot.st-open {
  background: #8cc265;
}
.st-dot.st-connecting {
  background: #d18f52;
  animation: pulse 1s ease-in-out infinite;
}
.st-dot.st-exited {
  background: #e05561;
}
.st-dot.st-closed {
  background: #6b6b7b;
}
@keyframes pulse {
  50% {
    opacity: 0.35;
  }
}

/* —— 快捷命令面板（Tab 条与终端之间；可折叠成单行头部） —— */
.quick-cmds {
  display: flex;
  flex-direction: column;
  background: #101016;
  border-bottom: 1px solid rgba(255, 255, 255, 0.08);
}
.quick-cmds.collapsed .qc-head {
  border-bottom: none;
}
.qc-head {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 5px 10px;
  border-bottom: 1px solid rgba(255, 255, 255, 0.06);
  flex-wrap: wrap;
}
.qc-toggle {
  border: none;
  background: none;
  color: #e4e4ec;
  font-size: 12px;
  font-weight: 600;
  cursor: pointer;
  padding: 3px 4px;
  border-radius: 6px;
}
.qc-toggle:hover {
  background: rgba(255, 255, 255, 0.08);
}
.qc-search {
  width: 150px;
  padding: 4px 9px;
  border-radius: 999px;
  border: 1px solid rgba(255, 255, 255, 0.14);
  background: rgba(0, 0, 0, 0.3);
  color: inherit;
  font-size: 11px;
}
.qc-search:focus {
  outline: none;
  border-color: rgba(122, 162, 247, 0.6);
}
.qc-pin {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  font-size: 11px;
  opacity: 0.8;
  cursor: pointer;
  user-select: none;
}
.qc-add {
  border: 1px dashed rgba(255, 255, 255, 0.25);
  background: none;
  color: inherit;
  font-size: 11px;
  padding: 3px 9px;
  border-radius: 999px;
  cursor: pointer;
}
.qc-add:hover {
  border-color: rgba(122, 162, 247, 0.6);
  background: rgba(122, 162, 247, 0.1);
}
.qc-tools {
  display: flex;
  align-items: center;
  gap: 5px;
  margin-left: auto;
}
.qc-tool {
  border: 1px solid rgba(255, 255, 255, 0.14);
  background: rgba(255, 255, 255, 0.05);
  color: inherit;
  font-size: 11px;
  padding: 3px 8px;
  border-radius: 6px;
  cursor: pointer;
}
.qc-tool:hover {
  background: rgba(255, 255, 255, 0.12);
}
.qc-font {
  font-size: 11px;
  opacity: 0.75;
  min-width: 32px;
  text-align: center;
}
.qc-body {
  display: flex;
  flex-direction: column;
  gap: 6px;
  padding: 6px 10px 8px;
}
.qc-form {
  display: flex;
  gap: 6px;
  align-items: center;
  flex-wrap: wrap;
}
.qc-form .input {
  width: auto;
  flex: 1 1 150px;
}
.qc-cats {
  display: flex;
  gap: 5px;
  flex-wrap: wrap;
}
.qc-cat {
  border: 1px solid rgba(255, 255, 255, 0.12);
  background: none;
  color: rgba(228, 228, 236, 0.7);
  font-size: 11px;
  padding: 2px 10px;
  border-radius: 999px;
  cursor: pointer;
}
.qc-cat:hover {
  color: #e4e4ec;
  background: rgba(255, 255, 255, 0.08);
}
.qc-cat.active {
  color: #e4e4ec;
  background: rgba(122, 162, 247, 0.2);
  border-color: rgba(122, 162, 247, 0.55);
}
.qc-list {
  display: flex;
  gap: 6px;
  flex-wrap: wrap;
}
.qc-cmd {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 4px 10px;
  border-radius: 8px;
  border: 1px solid rgba(255, 255, 255, 0.12);
  background: rgba(255, 255, 255, 0.05);
  color: #e4e4ec;
  font-size: 11px;
  cursor: pointer;
}
.qc-cmd:hover {
  background: rgba(140, 194, 101, 0.16);
  border-color: rgba(140, 194, 101, 0.45);
}
.qc-cmd-name {
  font-weight: 600;
}
.qc-cmd-cat {
  font-size: 10px;
  opacity: 0.5;
}
.qc-cmd-del {
  font-size: 13px;
  opacity: 0.55;
  padding: 0 2px;
  border-radius: 4px;
}
.qc-cmd-del:hover {
  opacity: 1;
  background: rgba(224, 85, 97, 0.3);
}
.qc-empty {
  font-size: 11px;
  padding: 4px 2px;
}

/* —— xterm 挂载点（黑色主题）—— */
.term-host {
  position: relative;
  flex: 1;
  min-height: 0;
  padding: 6px 8px;
}
.xterm-mount {
  width: 100%;
  height: 100%;
}
.term-error {
  position: absolute;
  left: 12px;
  bottom: 10px;
  right: 12px;
  padding: 8px 12px;
  border-radius: 8px;
  background: rgba(224, 85, 97, 0.15);
  border: 1px solid rgba(224, 85, 97, 0.4);
  color: #ffb3ba;
  font-size: 12px;
}

/* —— 输入控件（复用全站 .input 风格的局部变体）—— */
.input {
  width: 100%;
  box-sizing: border-box;
  padding: 7px 10px;
  border-radius: 8px;
  border: 1px solid rgba(255, 255, 255, 0.14);
  background: rgba(0, 0, 0, 0.25);
  color: inherit;
  font-size: 12px;
}
.input:focus {
  outline: none;
  border-color: rgba(122, 162, 247, 0.6);
}
.select {
  appearance: auto;
}
.btn {
  cursor: pointer;
  padding: 7px 12px;
  border-radius: 8px;
  border: 1px solid rgba(255, 255, 255, 0.14);
  background: rgba(255, 255, 255, 0.07);
  color: inherit;
  font-size: 12px;
}
.btn:hover:not(:disabled) {
  background: rgba(255, 255, 255, 0.12);
}
.btn:disabled {
  opacity: 0.45;
  cursor: not-allowed;
}
.btn-small {
  padding: 5px 10px;
}
.mono {
  font-family: 'Ubuntu Mono', Menlo, Consolas, monospace;
}
.muted {
  opacity: 0.65;
}
.form-msg {
  padding: 8px 12px;
  border-radius: 8px;
  font-size: 12px;
}
.form-msg.is-err {
  background: rgba(224, 85, 97, 0.12);
  border: 1px solid rgba(224, 85, 97, 0.35);
}
.form-msg.is-ok {
  background: rgba(140, 194, 101, 0.12);
  border: 1px solid rgba(140, 194, 101, 0.35);
}
.form-msg.is-info {
  background: rgba(122, 162, 247, 0.12);
  border: 1px solid rgba(122, 162, 247, 0.35);
}
</style>
