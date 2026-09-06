<script setup lang="ts">
// =============================================================================
// Network.vue —— 网络管理（Tab 分页：网络配置 / WAN 出口 / 防火墙 / P2P 节点网络 / BLE Mesh 中继）
//
// 功能：
//   0. 顶部 Tab 条（沿用 Provisioning/LlmModels 惯例，v-show 切换、DOM 保留不丢状态）
//      - Tab1 网络配置：状态概要 + 网卡列表（角色下拉）+ 路由/网关
//      - Tab2 WAN 出口：出口声明（offer）+ 授权表（node 下拉/TTL/撤销）+ 出口使用
//        （digest 学到的出口列表 → 设默认 + 本地 SOCKS5 地址一键复制 + 探活）
//      - Tab3 防火墙：规则表（方向/协议/端口/来源/动作/启用）+ 添加/删除/应用
//        （危险 confirm）+ iptables NEXOS-FW 链实况
//      - Tab4 P2P 节点网络：os-p2p 组网层观察面（状态卡/身份冲突警告条/节点表/桶图/阶梯统计）
//      - Tab5 BLE Mesh 中继：迁移自 Chat.vue 的「蓝牙 mesh 中继」Tab（BleHub.vue）
//   1. 顶部「刷新」按钮（统一刷新网络配置 Tab 三处数据）
//   2. 网卡列表：网卡名 + 状态徽章(UP 绿/DOWN 灰) + IP 地址 + 速率(Mbps→Gbps) + 类型
//   3. 路由/网关：默认网关 IP + 出接口
//   4. 网络状态概要：默认网关 / DNS / 网卡总数 / 在线数（卡片）
//
// API：
//   GET /api/v1/network/interfaces → 网卡列表（宽松字段解析，兼容多种形态）
//   GET /api/v1/network/routes     → 路由列表（提取默认路由）
//   GET /api/v1/network/status     → 网络状态概要
//   GET/POST /api/v1/net-exit/*    → WAN 出口共享（offer/authorize/use/proxy；503 → 引导）
//   GET/POST/DELETE /api/v1/firewall/* → 防火墙规则 + iptables 链实况
//   GET /api/v1/p2p/*              → P2P 组网层（5s 轮询、hidden 暂停）
//   GET/POST /api/v1/ble/*         → BLE mesh 中继（BleHub.vue 内部封装）
//
// 注：后端网络 API 字段尚未在 types.ts 固化，此处本地定义宽松类型 + 防御性
//     解析，对未知字段做 graceful fallback，避免后端结构微调即报错。
// =============================================================================
import { computed, onMounted, onUnmounted, ref } from 'vue';
import DataTable from '@/components/DataTable.vue';
import type { Column } from '@/components/data-table';
import { ApiError, endpoints } from '@/api/client';
import type {
  FirewallRule,
  FirewallStatusResp,
  NetExitStatus,
  P2pBucketsResp,
  P2pIdentityConflict,
  P2pLadderStats,
  P2pNodeMetaEntry,
  P2pPeer,
  P2pStatus,
} from '@/api/client';
import BleHub from '@/views/BleHub.vue';

// —— Tab 切换：网络配置 / WAN 出口 / 防火墙 / P2P 节点网络 / BLE Mesh 中继 ——

/** Tab key。 */
type TabKey = 'config' | 'wanexit' | 'firewall' | 'p2p' | 'ble';

/** Tab 定义（顺序即展示顺序）。 */
const tabs: { key: TabKey; label: string }[] = [
  { key: 'config', label: '网络配置' },
  { key: 'wanexit', label: 'WAN 出口' },
  { key: 'firewall', label: '防火墙' },
  { key: 'p2p', label: 'P2P 节点网络' },
  { key: 'ble', label: 'BLE Mesh 中继' },
];

/** 当前激活 Tab。 */
const activeTab = ref<TabKey>('config');

// —— 网卡角色标签（与后端 NicRole 枚举对齐，snake_case 单词）——

/** 网卡角色 key（与后端 snake_case 一致）。 */
type NicRole = 'normal' | 'management' | 'storage' | 'pxe' | 'dhcp' | 'dns';

/** 角色显示元数据（中文标签 + 徽章 CSS class）。 */
const ROLE_META: Record<NicRole, { label: string; cls: string }> = {
  normal: { label: '普通', cls: 'role-gray' },
  management: { label: '管理', cls: 'role-blue' },
  storage: { label: '存储', cls: 'role-cyan' },
  pxe: { label: 'PXE', cls: 'role-orange' },
  dhcp: { label: 'DHCP', cls: 'role-purple' },
  dns: { label: 'DNS', cls: 'role-pink' },
};

/** 下拉选项顺序。 */
const ROLE_OPTIONS: NicRole[] = ['normal', 'management', 'storage', 'pxe', 'dhcp', 'dns'];

/** 把任意后端值归一为合法 NicRole（未知/缺失 → normal）。 */
function normalizeRole(raw: unknown): NicRole {
  const s = String(raw ?? '').trim().toLowerCase();
  return (ROLE_OPTIONS as string[]).includes(s) ? (s as NicRole) : 'normal';
}

// —— 网络领域类型（本地宽松定义，对齐后端 snake_case 习惯，字段大多可选）——

/** 单条 IP 地址（可能为字符串或对象，后端形态不定）。 */
type IpEntry = string | { address?: string; addr?: string; ip?: string; prefix?: number };

/** 网卡接口。 */
interface NetInterface {
  name: string;
  is_up?: boolean;
  up?: boolean;
  state?: string;
  status?: string;
  ip_addresses?: IpEntry[];
  ips?: IpEntry[];
  addresses?: IpEntry[];
  speed_mbps?: number;
  speed?: number;
  type?: string;
  kind?: string;
  mac?: string;
  mac_address?: string;
  mtu?: number;
  /** 网卡角色标签（normal/management/storage/pxe/dhcp/dns），后端默认 normal。 */
  role?: string;
}

/** 路由条目。 */
interface NetRoute {
  destination?: string;
  dst?: string;
  gateway?: string;
  gw?: string;
  interface?: string;
  iface?: string;
  dev?: string;
  default?: boolean;
  is_default?: boolean;
  metric?: number;
}

/** 网络状态概要。 */
interface NetStatus {
  default_gateway?: string;
  gateway?: string;
  dns?: string[] | string;
  nameservers?: string[] | string;
  dns_servers?: string[] | string;
  interfaces_total?: number;
  total_interfaces?: number;
  interfaces_online?: number;
  online_interfaces?: number;
  online?: number;
  hostname?: string;
}

// —— 数据 ——
const interfaces = ref<NetInterface[]>([]);
const routes = ref<NetRoute[]>([]);
const status = ref<NetStatus | null>(null);

const loading = ref(false);
const interfacesError = ref('');
const routesError = ref('');
const statusError = ref('');

// —— 网卡角色编辑态 ——
/** 当前打开的角色下拉（网卡名 → true）。同一时刻一张网卡打开。 */
const roleOpenFor = ref<string | null>(null);
/** 正在提交角色的网卡名集合（禁用下拉，防重复提交）。 */
const roleSaving = ref<Set<string>>(new Set());
/** 角色保存错误（按网卡名）。 */
const roleError = ref<Record<string, string>>({});

// —— 加载函数 ——

/** 把后端返回的网卡列表规范化为 NetInterface[]。 */
function normalizeInterfaces(raw: unknown): NetInterface[] {
  if (!Array.isArray(raw)) return [];
  return raw.map((r) => (r && typeof r === 'object' ? (r as NetInterface) : { name: String(r) }));
}

/** 把后端返回的路由列表规范化为 NetRoute[]。 */
function normalizeRoutes(raw: unknown): NetRoute[] {
  if (!Array.isArray(raw)) return [];
  return raw
    .filter((r) => r && typeof r === 'object')
    .map((r) => r as NetRoute);
}

async function loadInterfaces(): Promise<void> {
  try {
    const v = await endpoints.networkInterfaces();
    interfaces.value = normalizeInterfaces(v);
    interfacesError.value = '';
  } catch (e) {
    interfacesError.value = e instanceof Error ? e.message : String(e);
    interfaces.value = [];
  }
}

async function loadRoutes(): Promise<void> {
  try {
    const v = await endpoints.networkRoutes();
    routes.value = normalizeRoutes(v);
    routesError.value = '';
  } catch (e) {
    routesError.value = e instanceof Error ? e.message : String(e);
    routes.value = [];
  }
}

async function loadStatus(): Promise<void> {
  try {
    const v = await endpoints.networkStatus();
    status.value = v && typeof v === 'object' ? (v as NetStatus) : null;
    statusError.value = '';
  } catch (e) {
    statusError.value = e instanceof Error ? e.message : String(e);
    status.value = null;
  }
}

/** 顶部「刷新」按钮：并行刷新三处数据。 */
async function refresh(): Promise<void> {
  loading.value = true;
  await Promise.allSettled([loadInterfaces(), loadRoutes(), loadStatus()]);
  loading.value = false;
}

// —— 字段访问辅助（兼容多种字段命名）——

/** 网卡是否处于 UP 状态。 */
function isUp(iface: NetInterface): boolean {
  if (typeof iface.is_up === 'boolean') return iface.is_up;
  if (typeof iface.up === 'boolean') return iface.up;
  const s = String(iface.state ?? iface.status ?? '').toLowerCase();
  return s === 'up' || s === 'online' || s === 'connected';
}

/** 取网卡上的 IP 地址字符串列表。 */
function getIps(iface: NetInterface): string[] {
  const raw = iface.ip_addresses ?? iface.ips ?? iface.addresses ?? [];
  if (!Array.isArray(raw)) return [];
  return raw
    .map((e) => {
      if (typeof e === 'string') return e;
      if (e && typeof e === 'object') {
        const a = String(e.address ?? e.addr ?? e.ip ?? '');
        return a;
      }
      return '';
    })
    .filter(Boolean);
}

/** 取网卡速率（Mbps）。 */
function getSpeedMbps(iface: NetInterface): number | null {
  const v = iface.speed_mbps ?? iface.speed;
  if (typeof v !== 'number' || !Number.isFinite(v) || v <= 0) return null;
  return v;
}

/** 速率格式化：1000 → "1 Gbps"，100 → "100 Mbps"。 */
function formatSpeed(iface: NetInterface): string {
  const mbps = getSpeedMbps(iface);
  if (mbps == null) return '—';
  if (mbps >= 1000) {
    const gbps = mbps / 1000;
    const txt = Number.isInteger(gbps) ? String(gbps) : gbps.toFixed(gbps < 10 ? 1 : 0);
    return `${txt} Gbps`;
  }
  return `${mbps} Mbps`;
}

/** 取网卡类型（以太网/WiFi/环回/…）。 */
function getType(iface: NetInterface): string {
  const raw = String(iface.type ?? iface.kind ?? '').toLowerCase();
  if (!raw) return '—';
  if (raw === 'ethernet' || raw === 'ether' || raw === 'eth') return '以太网';
  if (raw === 'wifi' || raw === 'wireless' || raw === 'wlan') return 'WiFi';
  if (raw === 'loopback' || raw === 'lo') return '环回';
  if (raw === 'bridge') return '桥接';
  if (raw === 'vlan') return 'VLAN';
  if (raw === 'bond') return '聚合';
  return raw;
}

/** 取 MAC 地址。 */
function getMac(iface: NetInterface): string {
  return String(iface.mac ?? iface.mac_address ?? '');
}

/** 取网卡角色（归一为 NicRole，缺失/未知 → normal）。 */
function getRole(iface: NetInterface): NicRole {
  return normalizeRole(iface.role);
}

/** 角色元数据查找（标签 + 徽章 class）。 */
function roleMeta(role: NicRole): { label: string; cls: string } {
  return ROLE_META[role];
}

/** 切换某网卡的角色下拉开合。 */
function toggleRoleMenu(name: string): void {
  roleOpenFor.value = roleOpenFor.value === name ? null : name;
  // 关闭时清该卡错误
  if (roleOpenFor.value !== name) delete roleError.value[name];
}

/** 关闭所有角色下拉（点击外部时调用）。 */
function closeAllRoleMenus(): void {
  roleOpenFor.value = null;
}

/** 选择新角色并立即提交保存。 */
async function chooseRole(iface: NetInterface, role: NicRole): Promise<void> {
  // 无变化只关菜单
  if (getRole(iface) === role) {
    roleOpenFor.value = null;
    return;
  }
  const name = iface.name;
  roleSaving.value = new Set(roleSaving.value).add(name);
  delete roleError.value[name];
  try {
    await endpoints.setNicRole(name, role);
    iface.role = role; // 本地立即生效
    roleOpenFor.value = null;
  } catch (e) {
    roleError.value[name] = e instanceof Error ? e.message : String(e);
  } finally {
    const next = new Set(roleSaving.value);
    next.delete(name);
    roleSaving.value = next;
  }
}

/** 取路由的默认网关 IP。 */
function getRouteGateway(r: NetRoute): string {
  return String(r.gateway ?? r.gw ?? '');
}

/** 取路由出接口。 */
function getRouteInterface(r: NetRoute): string {
  return String(r.interface ?? r.iface ?? r.dev ?? '');
}

/** 默认路由（destination 为 default 或标记 is_default）。 */
const defaultRoute = computed<NetRoute | null>(() => {
  return (
    routes.value.find(
      (r) => r.is_default === true || r.default === true,
    ) ??
    routes.value.find((r) => {
      const dst = String(r.destination ?? r.dst ?? '').toLowerCase();
      return dst === 'default' || dst === '0.0.0.0/0' || dst === '::/0';
    }) ??
    null
  );
});

/** 所有默认网关候选（用于状态卡片 + 路由区显示）。 */
function gatewayText(): string {
  if (status.value?.default_gateway) return String(status.value.default_gateway);
  if (status.value?.gateway) return String(status.value.gateway);
  const dr = defaultRoute.value;
  if (dr) return getRouteGateway(dr) || getRouteInterface(dr) || '—';
  return '—';
}

/** DNS 文本（逗号分隔）。 */
function dnsText(): string {
  const raw =
    status.value?.dns ??
    status.value?.nameservers ??
    status.value?.dns_servers;
  if (!raw) return '—';
  if (Array.isArray(raw)) return raw.length ? raw.join(', ') : '—';
  return String(raw) || '—';
}

/** 网卡总数。 */
function interfacesTotal(): number {
  if (typeof status.value?.interfaces_total === 'number') return status.value.interfaces_total;
  if (typeof status.value?.total_interfaces === 'number') return status.value.total_interfaces;
  return interfaces.value.length;
}

/** 在线网卡数。 */
function interfacesOnline(): number {
  if (typeof status.value?.interfaces_online === 'number') return status.value.interfaces_online;
  if (typeof status.value?.online_interfaces === 'number') return status.value.online_interfaces;
  if (typeof status.value?.online === 'number') return status.value.online;
  return interfaces.value.filter(isUp).length;
}

// =============================================================================
// 表格列定义
// =============================================================================
const ifaceColumns: Column<NetInterface>[] = [
  { key: 'name', title: '网卡名', accessor: (i) => i.name },
  { key: 'state', title: '状态', width: '90px' },
  { key: 'ip', title: 'IP 地址' },
  { key: 'speed', title: '速率', width: '110px', align: 'right' },
  { key: 'type', title: '类型', width: '110px' },
  { key: 'role', title: '角色', width: '120px' },
  { key: 'mac', title: 'MAC 地址', width: '160px' },
];

const routeColumns: Column<NetRoute>[] = [
  { key: 'destination', title: '目的网络' },
  { key: 'gateway', title: '网关', width: '180px' },
  { key: 'interface', title: '出接口', width: '140px' },
  { key: 'metric', title: 'Metric', width: '90px', align: 'right', sortable: true,
    accessor: (r) => r.metric },
  { key: 'default', title: '默认', width: '90px' },
];

onMounted(() => {
  void refresh();
  // WAN 出口 / 防火墙 Tab 数据（挂载即取一次——Tab 切换不重拉，按钮手动刷新）
  void refreshExitTab();
  void loadFwStatus();
  // P2P 轮询（5s；hidden 暂停，回前台由 visibilitychange 立即补一刷）
  void loadP2p();
  p2pTimer = setInterval(() => {
    if (document.hidden) return;
    void loadP2p();
  }, P2P_POLL_MS);
  document.addEventListener('visibilitychange', onP2pVisibility);
});

// =============================================================================
// WAN 出口（component network-exit；docs/NETWORK_EXIT_RELAY.md——v2ray 客户端
// 模式经自有加密 overlay：入口 SOCKS5 → net_exit 消息 → 出口节点代拨）
// =============================================================================

/** 出口状态（GET /api/v1/net-exit/status；503 → 未启用引导）。 */
const exitStatus = ref<NetExitStatus | null>(null);
/** 未启用（503 语义——NEXOS_P2P_ENABLE 未设）。 */
const exitDisabled = ref(false);
const exitError = ref('');
/** offer 切换中。 */
const exitOffering = ref(false);
/** 授权表单：node 下拉值 + TTL（分钟）。 */
const authNodeId = ref('');
const authTtl = ref(60);
const authSaving = ref(false);
/** 已知节点（node 下拉数据源：node-meta Active 条目）。 */
const knownNodes = ref<P2pNodeMetaEntry[]>([]);
/** 操作反馈（授权/撤销/设默认/探活）。 */
const exitMsg = ref<{ kind: 'err' | 'ok'; text: string } | null>(null);
/** 探活在途。 */
const probing = ref(false);
/** 复制反馈。 */
const copiedSocks = ref(false);

async function loadExit(): Promise<void> {
  if (exitDisabled.value) return;
  try {
    const st = await endpoints.netExitStatus();
    exitStatus.value = st;
    exitError.value = '';
    // node 下拉数据源（授权对象 = 已知节点；失败不阻断主状态）
    try {
      const meta = await endpoints.p2pNodeMeta();
      knownNodes.value = (Array.isArray(meta) ? meta : []).filter((m) =>
        'active' in (m.state ?? {}),
      );
    } catch {
      knownNodes.value = [];
    }
  } catch (e) {
    if (e instanceof ApiError && e.status === 503) {
      exitDisabled.value = true;
      return;
    }
    exitError.value = e instanceof Error ? e.message : String(e);
  }
}

/** 刷新 WAN 出口 Tab 三处数据（状态 + 防火墙规则）。 */
async function refreshExitTab(): Promise<void> {
  await Promise.allSettled([loadExit(), loadFirewallRules()]);
}

/** 切换本节点出口声明（POST /net-exit/offer）。 */
async function toggleExitOffer(): Promise<void> {
  if (!exitStatus.value) return;
  exitOffering.value = true;
  exitMsg.value = null;
  try {
    const next = !exitStatus.value.offered;
    await endpoints.netExitOffer(next);
    exitMsg.value = {
      kind: 'ok',
      text: next
        ? '已声明出口——下一轮元数据交互（≤6 tick）全网可发现'
        : '已撤销出口声明',
    };
    await loadExit();
  } catch (e) {
    exitMsg.value = { kind: 'err', text: '切换失败：' + (e instanceof Error ? e.message : String(e)) };
  } finally {
    exitOffering.value = false;
  }
}

/** 授权节点（POST /net-exit/authorize）。 */
async function authorizeNode(): Promise<void> {
  exitMsg.value = null;
  const id = authNodeId.value.trim();
  if (!id) {
    exitMsg.value = { kind: 'err', text: '请选择要授权的节点' };
    return;
  }
  authSaving.value = true;
  try {
    const r = await endpoints.netExitAuthorize(id, authTtl.value);
    exitMsg.value = {
      kind: 'ok',
      text: `已授权 ${shortId(id)}（至 ${new Date(r.expires_at * 1000).toLocaleString()}）`,
    };
    authNodeId.value = '';
    await loadExit();
  } catch (e) {
    exitMsg.value = { kind: 'err', text: '授权失败：' + (e instanceof Error ? e.message : String(e)) };
  } finally {
    authSaving.value = false;
  }
}

/** 撤销授权（DELETE /net-exit/authorize/:node_id）。 */
async function revokeAuth(nodeId: string): Promise<void> {
  exitMsg.value = null;
  try {
    await endpoints.netExitRevoke(nodeId);
    exitMsg.value = { kind: 'ok', text: `已撤销 ${shortId(nodeId)} 的出网授权` };
    await loadExit();
  } catch (e) {
    exitMsg.value = { kind: 'err', text: '撤销失败：' + (e instanceof Error ? e.message : String(e)) };
  }
}

/** 设为默认出口（POST /net-exit/use；null 清除）。 */
async function useExit(nodeId: string | null): Promise<void> {
  exitMsg.value = null;
  try {
    await endpoints.netExitUse(nodeId);
    exitMsg.value = {
      kind: 'ok',
      text: nodeId
        ? `默认出口 = ${shortId(nodeId)}——浏览器/系统代理指向 ${exitStatus.value?.local_socks ?? '127.0.0.1:11081'} 即经其出网`
        : '已清除默认出口',
    };
    await loadExit();
  } catch (e) {
    exitMsg.value = { kind: 'err', text: '设置失败：' + (e instanceof Error ? e.message : String(e)) };
  }
}

/** 经默认出口探活（POST /net-exit/proxy）。 */
async function probeExit(): Promise<void> {
  exitMsg.value = null;
  probing.value = true;
  try {
    const r = await endpoints.netExitProbe('1.1.1.1', 443);
    exitMsg.value = r.ok
      ? { kind: 'ok', text: `出口 ${shortId(r.exit_node)} 探活成功（TCP 1.1.1.1:443 代拨可达）` }
      : { kind: 'err', text: `探活失败（${shortId(r.exit_node || '无默认出口')}）：${r.error ?? ''}` };
  } catch (e) {
    exitMsg.value = { kind: 'err', text: '探活失败：' + (e instanceof Error ? e.message : String(e)) };
  } finally {
    probing.value = false;
  }
}

/** 一键复制本地 SOCKS5 地址。 */
async function copySocks(): Promise<void> {
  const addr = exitStatus.value?.local_socks ?? '127.0.0.1:11081';
  try {
    await navigator.clipboard.writeText(addr);
    copiedSocks.value = true;
    setTimeout(() => (copiedSocks.value = false), 1500);
  } catch {
    exitMsg.value = { kind: 'err', text: `复制失败——手动复制 ${addr}` };
  }
}

// —— 防火墙（规则 CRUD + apply + iptables 实况；空表起步无 seed）——

/** 规则列表。 */
const fwRules = ref<FirewallRule[]>([]);
const fwError = ref('');
const fwMsg = ref<{ kind: 'err' | 'ok'; text: string } | null>(null);
/** 添加表单（对话框）。 */
const fwShowAdd = ref(false);
const fwAddForm = ref({
  direction: 'in' as 'in' | 'out',
  proto: 'tcp' as 'tcp' | 'udp' | 'icmp' | 'any',
  port: '',
  source: 'any',
  action: 'allow' as 'allow' | 'deny',
  note: '',
});
const fwAdding = ref(false);
/** apply 在途。 */
const fwApplying = ref(false);
/** iptables 实况。 */
const fwChainStatus = ref<FirewallStatusResp | null>(null);

async function loadFirewallRules(): Promise<void> {
  try {
    const list = await endpoints.firewallRules();
    fwRules.value = Array.isArray(list) ? list : [];
    fwError.value = '';
  } catch (e) {
    fwError.value = e instanceof Error ? e.message : String(e);
    fwRules.value = [];
  }
}

/** 添加规则（deny + in + tcp/any + 22 + any → 后端 400 需 force——前端 confirm）。 */
async function addFwRule(): Promise<void> {
  fwMsg.value = null;
  const f = fwAddForm.value;
  const port = f.port.trim() === '' ? null : Number(f.port);
  if (port !== null && (!Number.isInteger(port) || port < 1 || port > 65535)) {
    fwMsg.value = { kind: 'err', text: '端口须为 1..=65535 或留空' };
    return;
  }
  const dangerous =
    f.action === 'deny' && f.direction === 'in' && (f.proto === 'tcp' || f.proto === 'any') &&
    port === 22 && (f.source.trim() === '' || f.source.trim() === 'any');
  if (dangerous && !confirm('这条规则会把 SSH（22/tcp）管理口对全网关死，确认要添加？')) {
    return;
  }
  fwAdding.value = true;
  try {
    await endpoints.firewallRuleAdd({
      direction: f.direction,
      proto: f.proto,
      port,
      source: f.source.trim() || 'any',
      action: f.action,
      note: f.note.trim(),
      force: dangerous,
    });
    fwMsg.value = { kind: 'ok', text: '规则已添加（点「应用」落到 iptables）' };
    fwShowAdd.value = false;
    fwAddForm.value = {
      direction: 'in', proto: 'tcp', port: '', source: 'any', action: 'allow', note: '',
    };
    await loadFirewallRules();
  } catch (e) {
    fwMsg.value = { kind: 'err', text: '添加失败：' + (e instanceof Error ? e.message : String(e)) };
  } finally {
    fwAdding.value = false;
  }
}

/** 启/停切换。 */
async function toggleFwRule(rule: FirewallRule): Promise<void> {
  fwMsg.value = null;
  try {
    await endpoints.firewallRuleToggle(rule.id, !rule.enabled);
    await loadFirewallRules();
  } catch (e) {
    fwMsg.value = { kind: 'err', text: '切换失败：' + (e instanceof Error ? e.message : String(e)) };
  }
}

/** 删除规则。 */
async function deleteFwRule(rule: FirewallRule): Promise<void> {
  fwMsg.value = null;
  if (!confirm(`删除规则 ${rule.id}（${rule.direction}/${rule.proto}${rule.port ?? ''} ${rule.action}）？`)) {
    return;
  }
  try {
    await endpoints.firewallRuleDelete(rule.id);
    await loadFirewallRules();
  } catch (e) {
    fwMsg.value = { kind: 'err', text: '删除失败：' + (e instanceof Error ? e.message : String(e)) };
  }
}

/** 危险规则判定（前端预判 + 后端复核）。 */
function fwIsDangerous(rule: FirewallRule): boolean {
  return (
    rule.action === 'deny' && rule.direction === 'in' &&
    (rule.proto === 'tcp' || rule.proto === 'any') && rule.port === 22 &&
    (rule.source === 'any' || rule.source === '')
  );
}

/** 应用规则到 iptables（NEXOS-FW / NEXOS-FW-OUT 自定义链，flush 先行）。 */
async function applyFw(): Promise<void> {
  fwMsg.value = null;
  const dangerous = fwRules.value.filter((r) => r.enabled && fwIsDangerous(r));
  if (dangerous.length && !confirm(
    `规则集含 ${dangerous.length} 条 deny-SSH 规则（${dangerous.map((r) => r.id).join(', ')}），应用后可能失去管理口连接。确认继续？`,
  )) {
    return;
  }
  fwApplying.value = true;
  try {
    const r = await endpoints.firewallApply(dangerous.length > 0);
    fwMsg.value = {
      kind: r.applied ? 'ok' : 'err',
      text: r.applied
        ? `已应用 ${r.rules_enabled}/${r.rules_total} 条规则到 ${r.chains.join(' + ')}`
        : `部分命令失败：${r.warning || '详见下方命令记录'}`,
    };
    await loadFwStatus();
  } catch (e) {
    fwMsg.value = { kind: 'err', text: '应用失败：' + (e instanceof Error ? e.message : String(e)) };
  } finally {
    fwApplying.value = false;
  }
}

/** 拉取 iptables 链实况。 */
async function loadFwStatus(): Promise<void> {
  try {
    fwChainStatus.value = await endpoints.firewallStatus();
  } catch (e) {
    fwChainStatus.value = null;
  }
}

// =============================================================================
// P2P 节点网络（os-p2p 组网层观察面；GET /api/v1/p2p/*，5s 轮询、hidden 暂停）
// =============================================================================

const p2pStatus = ref<P2pStatus | null>(null);
const p2pPeers = ref<P2pPeer[]>([]);
const p2pBuckets = ref<P2pBucketsResp | null>(null);
const p2pLadder = ref<P2pLadderStats | null>(null);
/** 身份冲突观测（同公钥多地址进入——仅提示不阻断；内存态，重启清除）。 */
const p2pConflicts = ref<P2pIdentityConflict[]>([]);
/** 未启用（503 语义）——展示开启指引而非报错。 */
const p2pDisabled = ref(false);
const p2pError = ref('');
/** 操作反馈（发送/连接）。 */
const p2pMsg = ref<{ kind: 'err' | 'ok'; text: string } | null>(null);

const p2pSendTo = ref('');
const p2pSendText = ref('');
const p2pSending = ref(false);
const p2pConnecting = ref<string | null>(null);

let p2pTimer: ReturnType<typeof setInterval> | null = null;
/** P2P 轮询间隔（ms）。 */
const P2P_POLL_MS = 5_000;

async function loadP2p(): Promise<void> {
  if (p2pDisabled.value) return; // 未启用不再轮询（重启 os-api 才会变化）
  try {
    const [st, peers, buckets, ladder, conflicts] = await Promise.all([
      endpoints.p2pStatus(),
      endpoints.p2pPeers(),
      endpoints.p2pBuckets(),
      endpoints.p2pLadder(),
      endpoints.p2pIdentityConflicts(),
    ]);
    p2pStatus.value = st;
    p2pPeers.value = Array.isArray(peers) ? peers : [];
    p2pBuckets.value = buckets;
    p2pLadder.value = ladder;
    p2pConflicts.value = Array.isArray(conflicts) ? conflicts : [];
    p2pError.value = '';
  } catch (e) {
    if (e instanceof ApiError && e.status === 503) {
      p2pDisabled.value = true; // 切换为引导文案并停止后续轮询
      return;
    }
    p2pError.value = e instanceof Error ? e.message : String(e);
  }
}

function p2pMsgFrom(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** 发送 P2P 消息（POST /api/v1/p2p/send，admin）。 */
async function sendP2pMessage(): Promise<void> {
  p2pMsg.value = null;
  const to = p2pSendTo.value.trim();
  const text = p2pSendText.value.trim();
  if (!to || !text) {
    p2pMsg.value = { kind: 'err', text: '请填写目标 NodeID 与消息内容' };
    return;
  }
  p2pSending.value = true;
  try {
    await endpoints.p2pSend(to, text);
    p2pMsg.value = { kind: 'ok', text: `已发送 → ${shortId(to)}（送达在对端体现）` };
    p2pSendText.value = '';
  } catch (e) {
    p2pMsg.value = { kind: 'err', text: '发送失败：' + p2pMsgFrom(e) };
  } finally {
    p2pSending.value = false;
  }
}

/** 对某节点主动走连接阶梯（POST /api/v1/p2p/connect，admin）。 */
async function connectP2pPeer(id: string): Promise<void> {
  p2pMsg.value = null;
  p2pConnecting.value = id;
  try {
    const r = await endpoints.p2pConnect(id);
    const pathLabel: Record<string, string> = {
      direct: '直连',
      punched: '打洞直连',
      relayed: '中继',
    };
    p2pMsg.value = {
      kind: 'ok',
      text: `${shortId(id)} 连接成功（${pathLabel[r.path] ?? r.path}）`,
    };
    await loadP2p();
  } catch (e) {
    p2pMsg.value = { kind: 'err', text: '连接失败：' + p2pMsgFrom(e) };
  } finally {
    p2pConnecting.value = null;
  }
}

/** NodeID/地址缩略（0x1234…cdef）。 */
function shortId(id: string): string {
  const s = String(id ?? '');
  return s.length > 14 ? `${s.slice(0, 8)}…${s.slice(-4)}` : s;
}

/** unix 秒 → 本地时间短格式（身份冲突观测时间展示）。 */
function fmtUnix(sec: number): string {
  return sec > 0 ? new Date(sec * 1000).toLocaleString() : '—';
}

/** 字节数 → 自适应可读（出口转发量展示）。 */
function fmtBytes(bytes: number): string {
  if (!bytes) return '0 B';
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KiB`;
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} MiB`;
  return `${(bytes / 1024 ** 3).toFixed(2)} GiB`;
}

/** 160 桶按 16 段聚合（每段 10 个邻域阶）——迷你条形图数据。 */
const p2pBucketSegments = computed<{ label: string; poFrom: number; poTo: number; count: number }[]>(() => {
  const counts = new Array<number>(16).fill(0);
  for (const b of p2pBuckets.value?.buckets ?? []) {
    const seg = Math.min(15, Math.max(0, Math.floor(b.po / 10)));
    counts[seg] += b.count;
  }
  return counts.map((count, i) => ({
    label: `${i * 10}`,
    poFrom: i * 10,
    poTo: i * 10 + 9,
    count,
  }));
});

/** 条形高度百分比（按段内最大值缩放，空表全 0）。 */
const p2pBucketMax = computed<number>(() =>
  Math.max(1, ...p2pBucketSegments.value.map((s) => s.count)),
);

/** 端点簿计数（地址交换所）。 */
const p2pEndpointCount = computed<number>(
  () => p2pBuckets.value?.known_endpoints?.length ?? 0,
);

/** 桶内节点总数。 */
const p2pBucketTotal = computed<number>(() =>
  p2pBucketSegments.value.reduce((acc, s) => acc + s.count, 0),
);

// —— P2P 轮询生命周期：挂载即起、卸载即停；document.hidden 时暂停 ——
function onP2pVisibility(): void {
  if (!document.hidden) void loadP2p(); // 回到页面立刻刷新一次
}

onUnmounted(() => {
  if (p2pTimer) clearInterval(p2pTimer);
  document.removeEventListener('visibilitychange', onP2pVisibility);
});

const p2pPeerColumns: Column<P2pPeer>[] = [
  { key: 'id', title: 'NodeID' },
  { key: 'underlay', title: 'Underlay' },
  { key: 'public', title: '角色', width: '110px' },
  { key: 'state', title: '连接态', width: '100px' },
  { key: 'actions', title: '操作', width: '90px' },
];
</script>

<template>
  <div class="network-page" @click="closeAllRoleMenus">
    <div class="page-head">
      <h2 class="page-title">网络管理</h2>
      <button class="btn btn-small" :disabled="loading" @click="refresh">
        <span class="spin" :class="{ spinning: loading }" aria-hidden="true">↻</span>
        刷新
      </button>
    </div>

    <!-- Tab 切换：网络配置 / 防火墙 / P2P 节点网络 / BLE Mesh 中继 -->
    <nav class="tabs" role="tablist">
      <button
        v-for="t in tabs"
        :key="t.key"
        class="tab"
        :class="{ active: activeTab === t.key }"
        role="tab"
        :aria-selected="activeTab === t.key"
        @click="activeTab = t.key"
      >{{ t.label }}</button>
    </nav>

    <!-- ====== Tab1 网络配置：状态概要 + 网卡接口 + 路由/网关（v-show 保留 DOM 状态）====== -->
    <div v-show="activeTab === 'config'" class="tab-body">
    <!-- 网络状态概要（卡片） -->
    <section class="panel">
      <div class="panel-head"><h3>网络状态概要</h3></div>
      <div v-if="statusError" class="error-box">状态加载失败：{{ statusError }}</div>
      <div class="stat-grid">
        <div class="card stat-card">
          <div class="stat-label">默认网关</div>
          <div class="stat-value mono">{{ gatewayText() }}</div>
          <div v-if="defaultRoute && getRouteInterface(defaultRoute)" class="stat-sub muted">
            出接口 {{ getRouteInterface(defaultRoute) }}
          </div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">DNS 服务器</div>
          <div class="stat-value mono">{{ dnsText() }}</div>
          <div v-if="status?.hostname" class="stat-sub muted">
            主机名 {{ status.hostname }}
          </div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">网卡总数</div>
          <div class="stat-value">{{ interfacesTotal() }}</div>
          <div class="stat-sub muted">块接口</div>
        </div>
        <div class="card stat-card">
          <div class="stat-label">在线网卡</div>
          <div class="stat-value is-ok">{{ interfacesOnline() }}</div>
          <div class="stat-sub muted">块 UP</div>
        </div>
      </div>
    </section>

    <!-- 网卡列表 -->
    <section class="panel">
      <div class="panel-head"><h3>网卡接口</h3></div>
      <div v-if="interfacesError" class="error-box">网卡加载失败：{{ interfacesError }}</div>
      <div class="card card-table">
        <DataTable
          :columns="ifaceColumns"
          :rows="interfaces"
          :loading="loading"
          empty-text="暂无网卡信息（后端可能尚未提供 /api/v1/network/interfaces）。"
        >
          <template #cell-state="{ row }">
            <span :class="['pill', isUp(row) ? 'pill-up' : 'pill-down']">
              {{ isUp(row) ? 'UP' : 'DOWN' }}
            </span>
          </template>
          <template #cell-ip="{ row }">
            <span v-if="getIps(row).length" class="mono ip-list">
              {{ getIps(row).join(', ') }}
            </span>
            <span v-else class="muted">—</span>
          </template>
          <template #cell-speed="{ row }">{{ formatSpeed(row) }}</template>
          <template #cell-type="{ row }">{{ getType(row) }}</template>
          <template #cell-role="{ row }">
            <div class="role-cell" @click.stop="closeAllRoleMenus">
              <button
                type="button"
                :class="['role-badge', roleMeta(getRole(row)).cls, { 'role-active': roleOpenFor === row.name }]"
                :disabled="roleSaving.has(row.name)"
                :title="roleSaving.has(row.name) ? '保存中…' : '点击修改角色'"
                @click.stop="toggleRoleMenu(row.name)"
              >
                <span v-if="roleSaving.has(row.name)" class="role-spin" aria-hidden="true">↻</span>
                {{ roleMeta(getRole(row)).label }}
                <span class="role-caret" aria-hidden="true">▾</span>
              </button>
              <ul
                v-if="roleOpenFor === row.name"
                class="role-menu"
                @click.stop
              >
                <li
                  v-for="opt in ROLE_OPTIONS"
                  :key="opt"
                  :class="{ 'role-selected': getRole(row) === opt }"
                  @click="chooseRole(row, opt)"
                >
                  <span :class="['role-dot', ROLE_META[opt].cls]" aria-hidden="true"></span>
                  {{ ROLE_META[opt].label }}
                  <span v-if="getRole(row) === opt" class="role-check" aria-hidden="true">✓</span>
                </li>
              </ul>
              <div v-if="roleError[row.name]" class="role-error">{{ roleError[row.name] }}</div>
            </div>
          </template>
          <template #cell-mac="{ row }">
            <span v-if="getMac(row)" class="mono">{{ getMac(row) }}</span>
            <span v-else class="muted">—</span>
          </template>
        </DataTable>
      </div>
    </section>

    <!-- 路由 / 默认网关 -->
    <section class="panel">
      <div class="panel-head"><h3>路由 / 网关</h3></div>
      <div v-if="routesError" class="error-box">路由加载失败：{{ routesError }}</div>
      <div class="card card-table">
        <DataTable
          :columns="routeColumns"
          :rows="routes"
          :loading="loading"
          empty-text="暂无路由信息（后端可能尚未提供 /api/v1/network/routes）。"
        >
          <template #cell-destination="{ row }">
            <span class="mono">{{ row.destination ?? row.dst ?? '—' }}</span>
          </template>
          <template #cell-gateway="{ row }">
            <span class="mono">{{ getRouteGateway(row) || '—' }}</span>
          </template>
          <template #cell-interface="{ row }">
            <span class="mono">{{ getRouteInterface(row) || '—' }}</span>
          </template>
          <template #cell-default="{ row }">
            <span :class="['pill', (row.is_default || row.default) ? 'pill-up' : 'pill-muted']">
              {{ (row.is_default || row.default) ? '默认' : '—' }}
            </span>
          </template>
        </DataTable>
      </div>
    </section>
    </div><!-- /Tab1 网络配置 -->

    <!-- ====== Tab2 WAN 出口（v2ray 客户端模式经自有加密 overlay；503 → 引导）====== -->
    <div v-show="activeTab === 'wanexit'" class="tab-body">
    <section class="panel">
      <div class="panel-head">
        <h3>WAN 出口</h3>
        <button class="btn btn-small" :disabled="exitOffering" @click="refreshExitTab">刷新</button>
      </div>

      <!-- 未启用：503 引导文案 -->
      <div v-if="exitDisabled" class="card placeholder-row">
        <div class="placeholder-icon exit-icon">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5"
            stroke-linecap="round" stroke-linejoin="round">
            <circle cx="12" cy="12" r="9" />
            <path d="M3 12h18M12 3c3 3.5 3 14 0 18M12 3c-3 3.5-3 14 0 18" />
          </svg>
        </div>
        <div class="placeholder-body">
          <h4 class="placeholder-title">WAN 出口未启用</h4>
          <p class="placeholder-text">
            出口共享依赖 P2P 组网层（加密 overlay 直连/打洞/中继）。设置
            <span class="mono">NEXOS_P2P_ENABLE=1</span> 并重启 os-api 后，本节点可声明
            出口供其他节点使用，也可把流量经其他出口节点出网（本地 SOCKS5）。
            详见 <span class="mono">docs/NETWORK_EXIT_RELAY.md</span>。
          </p>
        </div>
      </div>

      <template v-else>
        <div v-if="exitError" class="error-box">状态加载失败：{{ exitError }}</div>
        <div v-if="exitMsg" :class="['p2p-msg', exitMsg.kind === 'ok' ? 'p2p-msg-ok' : 'p2p-msg-err']">
          {{ exitMsg.text }}
        </div>

        <!-- 出口声明 + 本地 SOCKS5 入口卡 -->
        <div class="stat-grid">
          <div class="card stat-card">
            <div class="stat-label">本节点出口声明（offer）</div>
            <div class="stat-value">
              <span :class="['pill', exitStatus?.offered ? 'pill-public' : 'pill-muted']">
                {{ exitStatus?.offered ? '已声明' : '未声明' }}
              </span>
            </div>
            <button class="btn btn-small exit-offer-btn" :disabled="exitOffering" @click="toggleExitOffer">
              {{ exitOffering ? '切换中…' : exitStatus?.offered ? '撤销出口声明' : '声明为网络出口' }}
            </button>
            <div class="stat-sub muted">为 {{ exitStatus?.exit_for?.length ?? 0 }} 个已授权节点出网</div>
          </div>
          <div class="card stat-card">
            <div class="stat-label">本地 SOCKS5（入口）</div>
            <div class="stat-value mono">{{ exitStatus?.local_socks ?? '—' }}</div>
            <button class="btn btn-small" @click="copySocks">
              {{ copiedSocks ? '已复制 ✓' : '一键复制' }}
            </button>
            <div class="stat-sub muted">
              浏览器/系统代理填它（SOCKS5）即经默认出口出网
            </div>
          </div>
          <div class="card stat-card">
            <div class="stat-label">默认出口</div>
            <div class="stat-value mono" :title="exitStatus?.default_exit ?? ''">
              {{ exitStatus?.default_exit ? shortId(exitStatus.default_exit) : '未设置' }}
            </div>
            <button class="btn btn-small" :disabled="probing || !exitStatus?.default_exit" @click="probeExit">
              {{ probing ? '探活中…' : '探活（1.1.1.1:443）' }}
            </button>
            <div class="stat-sub muted">
              活跃中继 {{ exitStatus?.active_conns ?? 0 }} 条 · 已转发
              {{ fmtBytes(exitStatus?.stats?.bytes_relayed ?? 0) }}
            </div>
          </div>
        </div>

        <!-- 出口授权（默认 deny；node 下拉 = node-meta Active 条目） -->
        <div class="card exit-card">
          <div class="exit-card-title">出口授权（默认 deny——逐节点 TTL）</div>
          <div class="exit-auth-form">
            <select v-model="authNodeId" class="p2p-input exit-select">
              <option value="" disabled>选择节点（NodeID）</option>
              <option v-for="n in knownNodes" :key="n.id" :value="n.id">
                {{ shortId(n.id) }}（{{ 'active' in n.state ? `分数 ${n.state.active.score}` : '非活跃' }}）
              </option>
            </select>
            <input v-model.number="authTtl" class="p2p-input exit-ttl" type="number" min="1" max="43200" />
            <span class="muted exit-ttl-label">分钟</span>
            <button class="btn" :disabled="authSaving" @click="authorizeNode">
              {{ authSaving ? '授权中…' : '授权出网' }}
            </button>
          </div>
          <table v-if="exitStatus?.authorizations?.length" class="exit-table">
            <thead>
              <tr><th>节点</th><th>授权时刻</th><th>过期时刻</th><th>状态</th><th></th></tr>
            </thead>
            <tbody>
              <tr v-for="a in exitStatus.authorizations" :key="a.node_id">
                <td class="mono" :title="a.node_id">{{ shortId(a.node_id) }}</td>
                <td>{{ fmtUnix(a.granted_at) }}</td>
                <td>{{ fmtUnix(a.expires_at) }}</td>
                <td>
                  <span :class="['pill', a.expired ? 'pill-down' : 'pill-up']">
                    {{ a.expired ? '已过期' : '生效中' }}
                  </span>
                </td>
                <td>
                  <button class="btn btn-small" @click="revokeAuth(a.node_id)">撤销</button>
                </td>
              </tr>
            </tbody>
          </table>
          <div v-else class="muted exit-empty">暂无授权（其他节点默认不可经本节点出网）</div>
        </div>

        <!-- 出口使用（digest 学到的出口列表 → 设默认） -->
        <div class="card exit-card">
          <div class="exit-card-title">
            使用其他节点的出口
            <span class="muted exit-card-sub">经元数据交互学到的出口声明（exit_offered）</span>
          </div>
          <table v-if="exitStatus?.known_exits?.length" class="exit-table">
            <thead>
              <tr><th>出口节点</th><th>最近存活</th><th>状态</th><th></th></tr>
            </thead>
            <tbody>
              <tr v-for="k in exitStatus.known_exits" :key="k.node_id">
                <td class="mono" :title="k.node_id">{{ shortId(k.node_id) }}</td>
                <td>{{ fmtUnix(k.last_seen) }}</td>
                <td>
                  <span :class="['pill', k.alive ? 'pill-up' : 'pill-down']">
                    {{ k.alive ? '活跃' : '失联' }}
                  </span>
                </td>
                <td>
                  <button
                    v-if="exitStatus?.default_exit !== k.node_id"
                    class="btn btn-small"
                    @click="useExit(k.node_id)"
                  >设为默认出口</button>
                  <button v-else class="btn btn-small" @click="useExit(null)">取消默认</button>
                </td>
              </tr>
            </tbody>
          </table>
          <div v-else class="muted exit-empty">
            暂未发现出口节点——对端在网络管理里「声明为网络出口」后，经 1-2 轮元数据交互即可见
          </div>
        </div>
      </template>
    </section>
    </div><!-- /Tab2 WAN 出口 -->

    <!-- ====== Tab3 防火墙（规则 CRUD + iptables NEXOS-FW 链实况）====== -->
    <div v-show="activeTab === 'firewall'" class="tab-body">
    <section class="panel">
      <div class="panel-head">
        <h3>防火墙</h3>
        <div class="fw-head-actions">
          <button class="btn btn-small" @click="loadFwStatus">刷新实况</button>
          <button class="btn btn-small" @click="fwShowAdd = true">添加规则</button>
          <button class="btn btn-small fw-apply" :disabled="fwApplying" @click="applyFw">
            {{ fwApplying ? '应用中…' : '应用到 iptables' }}
          </button>
        </div>
      </div>
      <div v-if="fwError" class="error-box">规则加载失败：{{ fwError }}</div>
      <div v-if="fwMsg" :class="['p2p-msg', fwMsg.kind === 'ok' ? 'p2p-msg-ok' : 'p2p-msg-err']">
        {{ fwMsg.text }}
      </div>

      <!-- 添加对话框 -->
      <div v-if="fwShowAdd" class="card fw-add-card">
        <div class="exit-card-title">添加规则</div>
        <div class="fw-add-form">
          <label>方向
            <select v-model="fwAddForm.direction" class="p2p-input">
              <option value="in">入站（INPUT）</option>
              <option value="out">出站（OUTPUT）</option>
            </select>
          </label>
          <label>协议
            <select v-model="fwAddForm.proto" class="p2p-input">
              <option value="tcp">TCP</option>
              <option value="udp">UDP</option>
              <option value="icmp">ICMP</option>
              <option value="any">任意</option>
            </select>
          </label>
          <label>端口
            <input v-model="fwAddForm.port" class="p2p-input" type="text"
              placeholder="如 443（留空不限）" />
          </label>
          <label>源（CIDR）
            <input v-model="fwAddForm.source" class="p2p-input" type="text" placeholder="any 或 10.0.0.0/8" />
          </label>
          <label>动作
            <select v-model="fwAddForm.action" class="p2p-input">
              <option value="allow">允许（ACCEPT）</option>
              <option value="deny">拒绝（DROP）</option>
            </select>
          </label>
          <label>备注
            <input v-model="fwAddForm.note" class="p2p-input" type="text" placeholder="可选" />
          </label>
        </div>
        <div class="fw-add-actions">
          <button class="btn btn-small" @click="fwShowAdd = false">取消</button>
          <button class="btn btn-small fw-apply" :disabled="fwAdding" @click="addFwRule">
            {{ fwAdding ? '添加中…' : '添加' }}
          </button>
        </div>
      </div>

      <!-- 规则表 -->
      <div class="card card-table">
        <table class="fw-table">
          <thead>
            <tr>
              <th>ID</th><th>方向</th><th>协议</th><th>端口</th><th>来源</th>
              <th>动作</th><th>启用</th><th>备注</th><th></th>
            </tr>
          </thead>
          <tbody>
            <tr v-if="!fwRules.length">
              <td colspan="9" class="muted fw-empty">
                暂无规则——「添加规则」创建后点「应用到 iptables」生效
              </td>
            </tr>
            <tr v-for="r in fwRules" :key="r.id" :class="{ 'fw-danger': fwIsDangerous(r) }">
              <td class="mono">{{ r.id }}</td>
              <td>
                <span :class="['pill', r.direction === 'in' ? 'pill-up' : 'pill-public']">
                  {{ r.direction === 'in' ? '入站' : '出站' }}
                </span>
              </td>
              <td class="mono">{{ r.proto }}</td>
              <td class="mono">{{ r.port ?? '—' }}</td>
              <td class="mono">{{ r.source }}</td>
              <td>
                <span :class="['pill', r.action === 'allow' ? 'pill-up' : 'fw-pill-deny']">
                  {{ r.action === 'allow' ? '允许' : '拒绝' }}
                </span>
              </td>
              <td>
                <input type="checkbox" :checked="r.enabled" @change="toggleFwRule(r)" />
              </td>
              <td>{{ r.note || '—' }}</td>
              <td><button class="btn btn-small" @click="deleteFwRule(r)">删除</button></td>
            </tr>
          </tbody>
        </table>
      </div>

      <!-- iptables 链实况（sudo -L 回读） -->
      <div class="card exit-card" v-if="fwChainStatus">
        <div class="exit-card-title">
          iptables 实况
          <span class="muted exit-card-sub">{{ fwChainStatus.note }}</span>
        </div>
        <div v-for="(chain, name) in fwChainStatus.chains" :key="name" class="fw-chain">
          <div class="fw-chain-name mono">
            {{ name }}
            <span :class="['pill', chain.ok ? 'pill-up' : 'pill-down']">
              {{ chain.ok ? '可读' : '不可读' }}
            </span>
          </div>
          <pre v-if="chain.ok" class="fw-chain-raw mono">{{ chain.raw }}</pre>
          <div v-else class="muted">{{ chain.error }}</div>
        </div>
      </div>
    </section>
    </div><!-- /Tab3 防火墙 -->

    <!-- ====== Tab3 P2P 节点网络（os-p2p 组网层观察面）====== -->
    <div v-show="activeTab === 'p2p'" class="tab-body">
    <!-- P2P 节点网络（os-p2p 组网层：状态卡 / 节点表 / 桶可视化 / 阶梯统计） -->
    <section class="panel">
      <div class="panel-head"><h3>P2P 节点网络</h3></div>

      <!-- 未启用：503 引导文案（NEXOS_P2P_ENABLE=1 开启） -->
      <div v-if="p2pDisabled" class="card placeholder-row">
        <div class="placeholder-icon p2p-icon">
          <svg
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="1.5"
            stroke-linecap="round"
            stroke-linejoin="round"
          >
            <circle cx="12" cy="5" r="2.5" />
            <circle cx="5" cy="18" r="2.5" />
            <circle cx="19" cy="18" r="2.5" />
            <path d="M10.5 6.8 6.3 15.7M13.5 6.8l4.2 8.9M7.5 18h9" />
          </svg>
        </div>
        <div class="placeholder-body">
          <h4 class="placeholder-title">P2P 组网未启用</h4>
          <p class="placeholder-text">
            全分布式 Kademlia 组网层（节点发现 / 加密链路 / NAT 打洞 / 中继）默认关闭。
            在 os-api 环境 设置 <span class="mono">NEXOS_P2P_ENABLE=1</span>
            并重启后，本机即成为一个对等节点（可选
            <span class="mono">NEXOS_P2P_BOOTSTRAP=锚点:7070</span> 接入既有网络、
            <span class="mono">NEXOS_P2P_PUBLIC=1</span> 声明公网服务角色）。
          </p>
        </div>
      </div>

      <template v-else>
        <div v-if="p2pError" class="error-box">P2P 数据加载失败：{{ p2pError }}</div>

        <!-- 状态卡：自身身份 / 角色 / 监听 / 启用态 -->
        <div class="stat-grid">
          <div class="card stat-card">
            <div class="stat-label">本节点 NodeID</div>
            <div class="stat-value mono" :title="p2pStatus?.self?.node_id ?? ''">
              {{ p2pStatus?.self ? shortId(p2pStatus.self.node_id) : '—' }}
            </div>
            <div class="stat-sub muted mono">
              {{ p2pStatus?.self ? shortId(p2pStatus.self.overlay_addr) : '' }}
              <span class="stat-note">OverlayAddr</span>
            </div>
          </div>
          <div class="card stat-card">
            <div class="stat-label">昵称</div>
            <div class="stat-value">{{ p2pStatus?.self?.name || '（未命名）' }}</div>
            <div class="stat-sub muted">NEXOS_P2P_NAME</div>
          </div>
          <div class="card stat-card">
            <div class="stat-label">角色</div>
            <div class="stat-value">
              <span :class="['pill', p2pStatus?.self?.public ? 'pill-public' : 'pill-up']">
                {{ p2pStatus?.self?.public ? '公网服务节点' : '普通节点' }}
              </span>
            </div>
            <div class="stat-sub muted">
              {{ p2pStatus?.self?.public ? '锚点 + 端点交换所 + 中继' : 'NAT 后节点（打洞优先）' }}
            </div>
          </div>
          <div class="card stat-card">
            <div class="stat-label">监听地址</div>
            <div class="stat-value mono">{{ p2pStatus?.listen ?? '—' }}</div>
            <div class="stat-sub muted">
              已连接 {{ p2pStatus?.peers_connected ?? 0 }} / 已知 {{ p2pStatus?.peers_known ?? 0 }} 节点
            </div>
          </div>
        </div>

        <!-- 身份冲突警告条（同公钥从多个地址连接——仅提示，不阻断任何操作；
             身份=密钥：多个 OS 用同一私钥进入时权限共享是设计特性） -->
        <div v-if="p2pConflicts.length" class="p2p-conflict-box" role="alert">
          <div class="p2p-conflict-title">
            ⚠️ 检测到相同公钥从 {{ p2pConflicts.length }} 个地址连接
          </div>
          <ul class="p2p-conflict-list">
            <li v-for="c in p2pConflicts" :key="c.remote_addr">
              <span class="mono">{{ c.remote_addr }}</span>
              <span class="muted">
                （{{ shortId(c.node_id) }} · 警告 {{ c.warning_count }} 次 · 最近 {{ fmtUnix(c.last_seen) }}）
              </span>
            </li>
          </ul>
          <div class="p2p-conflict-note">
            多个 OS 使用同一私钥时权限共享（身份 = 密钥，设计特性）；不同 OS 的文件互不同步。
            本提示仅作本地告知，不阻断连接与消息。
          </div>
        </div>

        <!-- 节点表：peers 列表 -->
        <div class="card card-table">
          <DataTable
            :columns="p2pPeerColumns"
            :rows="p2pPeers"
            empty-text="暂无已知节点（配置 NEXOS_P2P_BOOTSTRAP 指向锚点，或等待 LAN mDNS 发现邻居）。"
          >
            <template #cell-id="{ row }">
              <span class="mono" :title="row.id">{{ shortId(row.id) }}</span>
            </template>
            <template #cell-underlay="{ row }">
              <span v-if="row.underlay" class="mono">{{ row.underlay }}</span>
              <span v-else class="muted">NAT（打洞/中继）</span>
            </template>
            <template #cell-public="{ row }">
              <span :class="['pill', row.public ? 'pill-public' : 'pill-muted']">
                {{ row.public ? '公网' : '普通' }}
              </span>
            </template>
            <template #cell-state="{ row }">
              <span :class="['pill', row.connected ? 'pill-up' : 'pill-down']">
                {{ row.connected ? '已连接' : '已知' }}
              </span>
            </template>
            <template #cell-actions="{ row }">
              <button
                class="btn btn-small"
                :disabled="p2pConnecting === row.id"
                @click="connectP2pPeer(row.id)"
              >
                {{ p2pConnecting === row.id ? '连接中…' : '连接' }}
              </button>
            </template>
          </DataTable>
        </div>

        <!-- 桶可视化 + 端点簿 / 阶梯统计 + 发消息 -->
        <div class="p2p-grid2">
          <div class="card p2p-card">
            <div class="p2p-card-title">
              Kademlia 桶占用
              <span class="muted p2p-card-sub">
                {{ p2pBucketTotal }} 节点 / {{ p2pBuckets?.buckets?.length ?? 0 }} 个非空桶 ·
                端点簿 {{ p2pEndpointCount }} 条
              </span>
            </div>
            <div class="bucket-bars" role="img" aria-label="k-bucket 占用条形图（160 桶按 16 段聚合）">
              <div
                v-for="seg in p2pBucketSegments"
                :key="seg.poFrom"
                class="bucket-bar"
                :title="`邻域阶 PO ${seg.poFrom}–${seg.poTo}：${seg.count} 节点（越近的桶越深）`"
              >
                <div
                  class="bucket-bar-fill"
                  :class="{ 'bucket-bar-near': seg.poFrom >= 80 }"
                  :style="{ height: `${Math.max(seg.count > 0 ? 6 : 0, (seg.count / p2pBucketMax) * 100)}%` }"
                ></div>
              </div>
            </div>
            <div class="bucket-axis muted">
              <span>PO 0（远）</span>
              <span>PO 159（近，每段 10 桶）</span>
            </div>
          </div>

          <div class="card p2p-card">
            <div class="p2p-card-title">
              连接阶梯统计
              <span class="muted p2p-card-sub">直连 → 打洞 → 中继</span>
            </div>
            <div class="ladder-row">
              <div class="ladder-item">
                <div class="ladder-num is-ok">{{ p2pLadder?.direct ?? 0 }}</div>
                <div class="ladder-label">Direct 直连</div>
              </div>
              <div class="ladder-item">
                <div class="ladder-num is-punch">{{ p2pLadder?.punched ?? 0 }}</div>
                <div class="ladder-label">Punched 打洞</div>
              </div>
              <div class="ladder-item">
                <div class="ladder-num is-relay">{{ p2pLadder?.relayed ?? 0 }}</div>
                <div class="ladder-label">Relayed 中继</div>
              </div>
              <div class="ladder-item">
                <div class="ladder-num">{{ p2pLadder?.punch_failed ?? 0 }}</div>
                <div class="ladder-label">打洞失败</div>
              </div>
            </div>

            <div class="p2p-divider"></div>

            <div class="p2p-card-title">发送消息（admin）</div>
            <div class="p2p-send-form">
              <input
                v-model="p2pSendTo"
                class="p2p-input mono"
                type="text"
                placeholder="目标 NodeID（0x…66 hex，可从上表复制）"
              />
              <input
                v-model="p2pSendText"
                class="p2p-input"
                type="text"
                placeholder="消息内容"
                @keyup.enter="sendP2pMessage"
              />
              <button class="btn" :disabled="p2pSending" @click="sendP2pMessage">
                {{ p2pSending ? '发送中…' : '发送' }}
              </button>
            </div>
            <div v-if="p2pMsg" :class="['p2p-msg', p2pMsg.kind === 'ok' ? 'p2p-msg-ok' : 'p2p-msg-err']">
              {{ p2pMsg.text }}
            </div>
          </div>
        </div>
      </template>
    </section>

    </div><!-- /Tab3 P2P 节点网络 -->

    <!-- ====== Tab4 BLE Mesh 中继（迁移自 Chat.vue 的「蓝牙 mesh 中继」Tab；BleHub.vue 原样嵌入，v-show 保留轮询/表单状态）====== -->
    <div v-show="activeTab === 'ble'" class="tab-body ble-pane">
      <BleHub />
    </div>
  </div>
</template>

<style scoped>
.network-page {
  padding: 20px 24px;
  display: flex;
  flex-direction: column;
  gap: 18px;
}

.page-head {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 12px;
}

.page-title {
  font-size: 22px;
  font-weight: 700;
  color: var(--text, #2B2B2B);
  letter-spacing: -0.02em;
}

/* —— Tab 条（沿用 Provisioning/LlmModels 惯例）—— */
.tabs {
  display: flex;
  gap: 4px;
  border-bottom: 1px solid var(--border, #EDEDED);
  flex-wrap: wrap;
}
.tab {
  padding: 8px 16px;
  background: transparent;
  border: none;
  border-bottom: 2px solid transparent;
  font-size: 14px;
  font-weight: 500;
  color: var(--text-muted, #5E5C5F);
  cursor: pointer;
  font-family: inherit;
  transition: color 0.15s ease, border-color 0.15s ease;
}
.tab:hover {
  color: var(--text, #2B2B2B);
}
.tab.active {
  color: var(--accent, #E95420);
  border-bottom-color: var(--accent, #E95420);
}

/* —— Tab 面板（与 .network-page 的区块间距一致）—— */
.tab-body {
  display: flex;
  flex-direction: column;
  gap: 18px;
}

/* BLE Mesh Tab：BleHub 根节点自带页面级 padding，嵌入 Tab 后归零对齐 */
.ble-pane :deep(.ble-page) {
  padding: 0;
}

/* —— 面板 —— */
.panel {
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.panel-head h3 {
  font-size: 16px;
  font-weight: 600;
  color: var(--text, #2B2B2B);
}

.card {
  background: var(--bg-card, #ffffff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}

.card-table {
  padding: 0;
  overflow: hidden;
}

.error-box {
  color: #b91c1c;
  background: #fee2e2;
  border: 1px solid rgba(185, 28, 28, 0.2);
  padding: 10px 14px;
  border-radius: var(--radius-sm, 8px);
  font-size: 13px;
}

/* —— 身份冲突警告条（同公钥多地址连接——仅提示不阻断）—— */
.p2p-conflict-box {
  color: #92400e;
  background: #fef3c7;
  border: 1px solid rgba(180, 130, 10, 0.35);
  padding: 10px 14px;
  border-radius: var(--radius-sm, 8px);
  font-size: 13px;
  margin-bottom: 14px;
}
.p2p-conflict-title {
  font-weight: 600;
  margin-bottom: 6px;
}
.p2p-conflict-list {
  margin: 0 0 6px;
  padding-left: 18px;
}
.p2p-conflict-list li {
  margin: 2px 0;
}
.p2p-conflict-note {
  font-size: 12px;
  color: #a16207;
}

/* —— 状态概要卡片网格 —— */
.stat-grid {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
  gap: 12px;
}

.stat-card {
  padding: 16px 18px;
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.stat-label {
  font-size: 12.5px;
  color: var(--text-muted, #5E5C5F);
  font-weight: 500;
}

.stat-value {
  font-size: 22px;
  font-weight: 700;
  color: var(--text, #2B2B2B);
  letter-spacing: -0.01em;
  word-break: break-all;
}

.stat-value.is-ok {
  color: #15803d;
}

.stat-sub {
  font-size: 12px;
  margin-top: 2px;
}

/* —— pill 徽章（UP/DOWN）—— */
.pill {
  display: inline-block;
  padding: 2px 10px;
  border-radius: var(--radius-pill, 20px);
  font-size: 12px;
  font-weight: 600;
  border: 1px solid transparent;
  white-space: nowrap;
}
.pill-up {
  color: #15803d;
  background: #dcfce7;
  border-color: rgba(21, 128, 61, 0.15);
}
.pill-down {
  color: #6b7280;
  background: #f3f4f6;
  border-color: rgba(107, 114, 128, 0.15);
}
.pill-muted {
  color: #6b7280;
  background: #f3f4f6;
}

/* —— 网卡角色徽章 + 下拉 —— */
.role-cell {
  position: relative;
  display: inline-block;
}

.role-badge {
  display: inline-flex;
  align-items: center;
  gap: 5px;
  padding: 3px 10px 3px 10px;
  border-radius: var(--radius-pill, 20px);
  font-size: 12px;
  font-weight: 600;
  border: 1px solid transparent;
  white-space: nowrap;
  cursor: pointer;
  font-family: inherit;
  transition: filter 0.15s ease, box-shadow 0.15s ease;
}
.role-badge:hover:not(:disabled) {
  filter: brightness(0.95);
}
.role-badge:disabled {
  opacity: 0.7;
  cursor: progress;
}
.role-badge.role-active {
  box-shadow: 0 0 0 2px rgba(59, 130, 246, 0.35);
}
.role-caret {
  font-size: 9px;
  opacity: 0.7;
  line-height: 1;
}
.role-spin {
  display: inline-block;
  font-size: 12px;
  line-height: 1;
  animation: spin 0.8s linear infinite;
}

/* 角色配色（管理=蓝 / 存储=青 / PXE=橙 / DHCP=紫 / DNS=粉 / 普通=灰） */
.role-blue {
  color: #C7421A;
  background: #dbeafe;
  border-color: rgba(29, 78, 216, 0.18);
}
.role-orange {
  color: #c2410c;
  background: #ffedd5;
  border-color: rgba(194, 65, 12, 0.18);
}
.role-pink {
  color: #be185d;
  background: #fce7f3;
  border-color: rgba(190, 24, 93, 0.18);
}
.role-purple {
  color: #7e22ce;
  background: #f3e8ff;
  border-color: rgba(126, 34, 206, 0.18);
}
.role-cyan {
  color: #0e7490;
  background: #cffafe;
  border-color: rgba(14, 116, 144, 0.18);
}
.role-gray {
  color: #6b7280;
  background: #f3f4f6;
  border-color: rgba(107, 114, 128, 0.18);
}

/* 下拉菜单 */
.role-menu {
  position: absolute;
  z-index: 30;
  top: calc(100% + 4px);
  left: 0;
  min-width: 140px;
  margin: 0;
  padding: 4px;
  list-style: none;
  background: var(--bg-card, #ffffff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-sm, 8px);
  box-shadow: 0 8px 24px rgba(0, 0, 0, 0.16);
}
.role-menu li {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 10px;
  font-size: 13px;
  color: var(--text, #2B2B2B);
  border-radius: 6px;
  cursor: pointer;
  user-select: none;
}
.role-menu li:hover {
  background: rgba(0, 0, 0, 0.05);
}
.role-menu li.role-selected {
  font-weight: 600;
  color: #C7421A;
}
.role-check {
  margin-left: auto;
  font-size: 12px;
}
.role-dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  display: inline-block;
  flex-shrink: 0;
}
/* dot 复用 badge 配色（取背景色作 dot） */
.role-dot.role-blue { background: #C7421A; }
.role-dot.role-orange { background: #c2410c; }
.role-dot.role-pink { background: #be185d; }
.role-dot.role-purple { background: #7e22ce; }
.role-dot.role-cyan { background: #0e7490; }
.role-dot.role-gray { background: #6b7280; }

.role-error {
  position: absolute;
  top: calc(100% + 4px);
  left: 0;
  font-size: 11px;
  color: #b91c1c;
  background: #fee2e2;
  padding: 2px 6px;
  border-radius: 4px;
  white-space: nowrap;
  z-index: 31;
}

.ip-list {
  font-size: 13px;
  color: var(--text, #2B2B2B);
}

/* —— 按钮 —— */
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

/* —— P2P 节点网络 —— */
.pill-public {
  color: #9a3412;
  background: #ffedd5;
  border-color: rgba(154, 52, 18, 0.18);
}
.p2p-icon {
  background: linear-gradient(135deg, #6366f1 0%, #a5b4fc 100%);
  box-shadow: 0 6px 16px rgba(99, 102, 241, 0.28);
}
.stat-note {
  font-size: 10.5px;
  opacity: 0.7;
  margin-left: 4px;
}
.p2p-grid2 {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(320px, 1fr));
  gap: 12px;
}
.p2p-card {
  padding: 16px 18px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.p2p-card-title {
  font-size: 14px;
  font-weight: 600;
  color: var(--text, #2B2B2B);
  display: flex;
  align-items: baseline;
  gap: 8px;
  flex-wrap: wrap;
}
.p2p-card-sub {
  font-size: 12px;
  font-weight: 400;
}

/* 桶占用迷你条形图（160 桶 → 16 段，每段 10 桶） */
.bucket-bars {
  display: flex;
  align-items: flex-end;
  gap: 3px;
  height: 84px;
  padding: 4px 2px 0;
}
.bucket-bar {
  flex: 1;
  height: 100%;
  display: flex;
  align-items: flex-end;
  background: rgba(0, 0, 0, 0.045);
  border-radius: 3px 3px 0 0;
  overflow: hidden;
}
.bucket-bar-fill {
  width: 100%;
  background: #30b0c7;
  border-radius: 3px 3px 0 0;
  min-height: 0;
  transition: height 0.3s ease;
}
.bucket-bar-fill.bucket-bar-near {
  background: #C7421A;
}
.bucket-axis {
  display: flex;
  justify-content: space-between;
  font-size: 11px;
}

/* 阶梯统计 */
.ladder-row {
  display: grid;
  grid-template-columns: repeat(4, 1fr);
  gap: 8px;
}
.ladder-item {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 2px;
  padding: 10px 4px;
  background: rgba(0, 0, 0, 0.025);
  border-radius: var(--radius-sm, 8px);
}
.ladder-num {
  font-size: 20px;
  font-weight: 700;
  color: var(--text, #2B2B2B);
}
.ladder-num.is-ok {
  color: #15803d;
}
.ladder-num.is-punch {
  color: #0e7490;
}
.ladder-num.is-relay {
  color: #C7421A;
}
.ladder-label {
  font-size: 11.5px;
  color: var(--text-muted, #5E5C5F);
}
.p2p-divider {
  height: 1px;
  background: var(--border, #D9D9D9);
  opacity: 0.6;
}

/* 发消息表单 */
.p2p-send-form {
  display: flex;
  gap: 8px;
  flex-wrap: wrap;
}
.p2p-input {
  flex: 1 1 180px;
  padding: 6px 10px;
  border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px);
  font-size: 13px;
  font-family: inherit;
  background: var(--bg-card, #ffffff);
  color: var(--text, #2B2B2B);
}
.p2p-input:focus {
  outline: 2px solid rgba(48, 176, 199, 0.35);
  outline-offset: 1px;
}
.p2p-msg {
  font-size: 12.5px;
  padding: 6px 10px;
  border-radius: var(--radius-sm, 8px);
}
.p2p-msg-ok {
  color: #15803d;
  background: #dcfce7;
}
.p2p-msg-err {
  color: #b91c1c;
  background: #fee2e2;
}

.spin {
  display: inline-block;
  font-size: 14px;
  line-height: 1;
}
.spin.spinning {
  animation: spin 0.8s linear infinite;
}
@keyframes spin {
  to {
    transform: rotate(360deg);
  }
}

/* —— WAN 出口 / 防火墙 —— */
.exit-icon {
  background: linear-gradient(135deg, #f97316 0%, #fdba74 100%);
  box-shadow: 0 6px 16px rgba(249, 115, 22, 0.28);
}
.exit-offer-btn {
  margin-top: 6px;
  align-self: flex-start;
}
.exit-card {
  padding: 16px 18px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.exit-card-title {
  font-size: 14px;
  font-weight: 600;
  color: var(--text, #2B2B2B);
  display: flex;
  align-items: baseline;
  gap: 8px;
  flex-wrap: wrap;
}
.exit-card-sub {
  font-size: 12px;
  font-weight: 400;
}
.exit-auth-form {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}
.exit-select {
  flex: 1 1 260px;
}
.exit-ttl {
  width: 90px;
}
.exit-ttl-label {
  font-size: 12.5px;
}
.exit-table,
.fw-table {
  width: 100%;
  border-collapse: collapse;
  font-size: 13px;
}
.exit-table th,
.fw-table th {
  text-align: left;
  font-size: 12px;
  font-weight: 600;
  color: var(--text-muted, #5E5C5F);
  padding: 8px 10px;
  border-bottom: 1px solid var(--border, #EDEDED);
  white-space: nowrap;
}
.exit-table td,
.fw-table td {
  padding: 8px 10px;
  border-bottom: 1px solid var(--border, #F4F4F4);
  vertical-align: middle;
}
.exit-table tr:last-child td,
.fw-table tr:last-child td {
  border-bottom: none;
}
.exit-empty,
.fw-empty {
  padding: 6px 2px;
}
.fw-head-actions {
  display: flex;
  gap: 8px;
}
.fw-apply {
  color: #b91c1c;
  border-color: rgba(185, 28, 28, 0.35);
}
.fw-pill-deny {
  color: #b91c1c;
  background: #fee2e2;
  border-color: rgba(185, 28, 28, 0.15);
}
tr.fw-danger td {
  background: #fff7f7;
}
.fw-add-card {
  padding: 16px 18px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.fw-add-form {
  display: grid;
  grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
  gap: 10px;
}
.fw-add-form label {
  display: flex;
  flex-direction: column;
  gap: 4px;
  font-size: 12.5px;
  color: var(--text-muted, #5E5C5F);
}
.fw-add-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}
.fw-chain {
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.fw-chain-name {
  font-weight: 600;
  display: flex;
  align-items: center;
  gap: 8px;
}
.fw-chain-raw {
  margin: 0;
  padding: 10px 12px;
  background: rgba(0, 0, 0, 0.035);
  border-radius: var(--radius-sm, 8px);
  font-size: 12px;
  max-height: 220px;
  overflow: auto;
  white-space: pre;
}

/* —— 占位行 —— */
.placeholder-row {
  padding: 24px 28px;
  display: flex;
  align-items: center;
  gap: 18px;
}

.placeholder-icon {
  width: 52px;
  height: 52px;
  border-radius: 14px;
  display: flex;
  align-items: center;
  justify-content: center;
  background: linear-gradient(135deg, #30b0c7 0%, #66d6e7 100%);
  color: #ffffff;
  box-shadow: 0 6px 16px rgba(48, 176, 199, 0.28);
  flex-shrink: 0;
}
.placeholder-icon svg {
  width: 30px;
  height: 30px;
}

.placeholder-body {
  display: flex;
  flex-direction: column;
  gap: 4px;
}

.placeholder-title {
  font-size: 16px;
  font-weight: 700;
  color: var(--text, #2B2B2B);
  letter-spacing: -0.01em;
}

.placeholder-text {
  font-size: 13px;
  line-height: 1.6;
  color: var(--text-muted, #5E5C5F);
  margin: 0;
}

.mono {
  font-family: var(--mono, monospace);
  font-size: 13px;
}

.muted {
  color: var(--text-muted, #5E5C5F);
  font-size: 13px;
}

@media (max-width: 720px) {
  .network-page {
    padding: 16px;
  }
}
</style>
