// =============================================================================
// 类型别名 barrel：把 "@/types" 聚合到 scaffold 实际定义在 "@/api/types" 的接口。
//
// 背景： Shares.vue / Users.vue / Nodes.vue / Settings.vue 是从 static/js 移植的
// 富功能页面，使用 ShareInfo / UserInfo / PeerNode / NodeCapabilities /
// SystemSettings / VirtCheckResult 等领域命名；scaffold 的强类型定义在
// "@/api/types"（Share / User / NodeInfo 等）。这里以别名 + 局部扩展桥接两套
// 命名，避免逐文件改写并保持运行时不变。
// =============================================================================

export type {
  Health,
  Capacity,
  VdevKind,
  VdevSpec,
  Vdev,
  Pool,
  EncryptionState,
  Dataset,
  Snapshot,
  CreatePoolRequest,
  VmState,
  NicModel,
  VmNic,
  VmFirmware,
  CpuTopology,
  VmSpec,
  Vm,
  CreateVmRequest,
  Share,
  NfsExport,
  Role,
  User,
  NodeRole,
  NodeInfo,
  CpuVirt,
  SystemStatus,
  VersionInfo,
} from './api/types';

// —— 共享（移植自 ShareInfo；字段与 Share 完全一致）——
export type ShareInfo = import('./api/types').Share;

// —— 用户（UserInfo 比 User 多 is_guest；移植自 users.js）——
export interface UserInfo {
  id: string;
  name: string;
  roles: string[];
  enabled: boolean;
  is_guest?: boolean;
  created_at?: string;
}

// —— 节点（PeerNode 移植自 static/js/nodes.js；字段 best-effort）——
/** 节点能力声明（LAN 发现包中的能力字段）。 */
export interface NodeCapabilities {
  has_kvm?: boolean;
  has_zfs?: boolean;
  rdma?: boolean;
  dpu?: boolean;
  supports_ha?: boolean;
  storage_capacity_gb?: number | null;
  network_gbps?: number | string | null;
  [key: string]: unknown;
}

/** LAN 内发现的节点（discover beacon）。 */
export interface PeerNode {
  node_id: string;
  role?: string;
  version?: string;
  arch?: string;
  endpoints?: string[];
  capabilities?: NodeCapabilities;
  beacon_signature?: string;
  [key: string]: unknown;
}

// —— 系统设置（前端本地，localStorage 持久化；Settings.vue）——
export interface SystemSettingsAdmin {
  name?: string;
  email?: string;
}

export interface SystemSettings {
  osName: string;
  language: string;
  timezone: string;
  admin: SystemSettingsAdmin;
}

// —— CPU 虚拟化检测结果（移植自 os-compute::virt_check::VirtCheckResult）——
//
// 后端 `CpuVendor` / `NestedVirtStatus` 枚举使用 serde 默认（externally-tagged）
// 序列化，因此 JSON 形如：
//   cpu_vendor: "Intel"                 ← CpuVendor::Intel
//   cpu_vendor: "Amd"                   ← CpuVendor::Amd
//   cpu_vendor: { "Unknown": "GenuineCPU" }  ← CpuVendor::Unknown(String)
//   nested_virt: { "Supported": true }  ← NestedVirtStatus::Supported(true)
//   nested_virt: { "Supported": false } ← NestedVirtStatus::Supported(false)
//   nested_virt: "Unknown"              ← NestedVirtStatus::Unknown
// 前端格式化（cpuVendorLabel / nestedVirtLabel）已对以上形态做归一化处理。
export type CpuVendorJson = 'Intel' | 'Amd' | { Unknown: string };

export type NestedVirtStatusJson =
  | { Supported: boolean }
  | 'Unknown';

export interface VirtCheckResult {
  cpu_vendor?: CpuVendorJson;
  cpu_has_virt_flags?: boolean;
  kvm_device_present?: boolean;
  kvm_module_loaded?: boolean;
  nested_virt?: NestedVirtStatusJson;
  /** 综合判定（虚拟化是否可用）；后端 is_usable() 派生字段。 */
  is_usable?: boolean;
  /** 用户友好的中文诊断文本（后端 to_user_diagnostic() 派生字段）。 */
  diagnostic?: string;
  [key: string]: unknown;
}
