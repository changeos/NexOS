// =============================================================================
// OS System 领域类型定义（TypeScript 接口，对齐后端 Rust serde 序列化结构）
//
// 字段命名与 Rust struct 的 serde 输出一致（snake_case；枚举经
// #[serde(rename_all = "snake_case"|"lowercase")] 转换）。
// 参考：
//   - crates/os-core/src/types.rs         (Health / Capacity / NodeInfo)
//   - crates/os-storage/src/model.rs      (Pool / Dataset / Snapshot / Vdev)
//   - crates/os-compute/src/vm.rs         (Vm / VmSpec / CpuTopology / VmState)
//   - crates/os-api/src/handlers/share.rs (ShareInfo / NfsExport)
//   - crates/os-api/src/handlers/system.rs (SystemStatus 系列字段)
//   - crates/os-api/src/handlers/discover.rs (NodeInfo)
// =============================================================================

// —— 健康状态（os-core::Health，snake_case 序列化）——
export type Health = 'healthy' | 'degraded' | 'unhealthy' | 'unknown';

// —— 容量（os-core::Capacity）——
export interface Capacity {
  used_bytes: number;
  total_bytes: number;
}

/** 计算使用率（0~1）；total=0 时返回 0。 */
export function usedRatio(cap: Capacity | undefined | null): number {
  if (!cap || !cap.total_bytes) return 0;
  return Math.max(0, Math.min(1, cap.used_bytes / cap.total_bytes));
}

// ============================================================================
// 存储（os-storage::model）
// ============================================================================

/** vdev 冗余级别（VdevKind，snake_case）。 */
export type VdevKind = 'disk' | 'mirror' | 'raidz1' | 'raidz2' | 'raidz3';

/** vdev 角色：data=数据盘 / special=特殊 vdev（加速 metadata、小文件）/ log=SLOG 同步写日志 / cache=L2ARC 二级读缓存。 */
export type VdevRole = 'data' | 'special' | 'log' | 'cache';

/** vdev 规格（创建池时声明）。 */
export interface VdevSpec {
  kind: VdevKind;
  disks: string[];
  /** vdev 角色；缺省为 'data'。special/log 用于 ZFS special vdev / ZIL 加速层。 */
  role?: VdevRole;
}

/** vdev 运行时实例（来自 zpool status）。 */
export interface Vdev {
  kind: VdevKind;
  disks: string[];
  health: Health;
  read_errors: number;
  write_errors: number;
  cksum_errors: number;
}

/** ZFS 存储池。 */
export interface Pool {
  id: string;
  name: string;
  vdevs: Vdev[];
  capacity: Capacity;
  health: Health;
}

/** 数据集加密状态（EncryptionState，snake_case）。 */
export type EncryptionState = 'off' | 'unlocked' | 'locked';

/** 数据集。 */
export interface Dataset {
  id: string;
  pool: string;
  name: string;
  used_bytes: number;
  avail_bytes: number;
  mounted: boolean;
  encryption: EncryptionState;
}

/** 快照（created 是 RFC3339 字符串，因 JSON 无原生日期类型）。 */
export interface Snapshot {
  id: string;
  dataset: string;
  /** RFC3339 / ISO8601 字符串。 */
  created: string;
  used_bytes: number;
}

/** 创建池请求体（POST /api/v1/pools）。 */
export interface CreatePoolRequest {
  name: string;
  vdevs: VdevSpec[];
}

/**
 * 本机可用磁盘（GET /api/v1/disks）。
 *
 * 后端通过 lsblk 探测，已过滤掉系统盘（挂载了 /、/boot*、swap）、loop 设备与
 * 已属 ZFS 池的整盘。`available` 恒为 true（不满足条件的盘不会出现在列表中）。
 */
export interface DiskInfo {
  /** 完整设备路径，如 `/dev/sda` / `/dev/nvme0n1`。 */
  name: string;
  /** 磁盘总容量（字节）。 */
  size_bytes: number;
  /** 磁盘型号字符串（可能为空）。 */
  model: string;
  /** 是否可用（恒为 true，仅作显式标记）。 */
  available: boolean;
  /**
   * 是否残留分区表或文件系统签名（如 BitLocker / GPT / MBR / ext4）。
   * true = 需先初始化（POST /api/v1/disks/:name/initialize，wipefs -a
   * 两步确认）才能加入新池；缺省视为 false（兼容旧后端）。
   */
  has_partitions?: boolean;
  /** 检测到的签名类型列表（保序去重；空 = 干净盘）。 */
  signatures?: string[];
  /**
   * 活跃（已导入）池成员盘：所属池名（zpool status config 解析）。
   * 有值的盘**永不**提示初始化——删除该池后才能重新初始化。
   * 缺省/null = 不属于任何活跃池。
   */
  member_of?: string | null;
  /**
   * 可导入（已导出/未导入）池提示：签名含 zfs_member 且被 `zpool import`
   * 列表命中时为池名。有值的盘**永不**提示初始化——数据没丢，导入即恢复
   * （POST /api/v1/disks/import）。缺省/null = 无。
   */
  zfs_pool_hint?: string | null;
}

/** GET /api/v1/disks/:name/partitions 的单个分区明细。 */
export interface DiskPartitionEntry {
  /** 分区设备名（如 nvme1n1p3）。 */
  name: string;
  /** 人类可读大小（lsblk 原文，如 "800G"）。 */
  size: string;
  /** 文件系统/签名类型（BitLocker / ext4 / vfat…；null = 无）。 */
  fstype?: string | null;
  /** 文件系统卷标（null = 无）。 */
  label?: string | null;
}

/** GET /api/v1/disks/:name/partitions 响应（创建池向导判断是否需初始化）。 */
export interface DiskPartitions {
  /** 整盘裸名（如 nvme1n1）。 */
  disk: string;
  /** 是否有分区表/签名（true = 前端禁选，需先初始化）。 */
  has_partitions: boolean;
  /** fstype 签名汇总。 */
  signatures: string[];
  /** 子分区明细（递归展平嵌套容器）。 */
  partitions: DiskPartitionEntry[];
  /** 降级提示（设备不存在 / lsblk 失败时后端仍 200 返回）。 */
  warning?: string;
}

/** POST /api/v1/disks/:name/initialize 响应（admin；wipefs -a 清除全部分区表/签名）。 */
export interface InitializeDiskResp {
  ok: boolean;
  /** 整盘裸名（回显）。 */
  disk: string;
  /** 本次清除的签名类型列表（来自 wipefs 预扫描）。 */
  wiped: string[];
}

// ============================================================================
// 虚拟机（os-compute::vm）
// ============================================================================

/** VM 运行状态（VmState，snake_case）。 */
export type VmState = 'running' | 'paused' | 'stopped' | 'failed' | 'migrating';

/** 网卡模型（NicModel，lowercase）。 */
export type NicModel = 'virtio' | 'e1000';

/** 网卡。 */
export interface VmNic {
  bridge: string;
  mac?: string;
  model: NicModel;
}

/** 固件类型（VmFirmware，lowercase）。 */
export type VmFirmware = 'bios' | 'uefi';

/** CPU 拓扑。 */
export interface CpuTopology {
  vcpus: number;
  sockets: number;
  cores: number;
  threads: number;
}

/** VM 规格（创建时声明，POST /api/v1/vms 的 body）。 */
export interface VmSpec {
  cpus: CpuTopology;
  memory_mb: number;
  disk_vol_id: string;
  nics: VmNic[];
  firmware: VmFirmware;
}

/** VM 实例（GET /api/v1/vms 返回元素）。 */
export interface Vm {
  id: string;
  name: string;
  spec: VmSpec;
  state: VmState;
  /** 运行所在节点（迁移时会变化；undefined = 未调度）。 */
  node_id?: string;
  /** 创建时间（RFC3339）。 */
  created_at: string;
}

/** 创建 VM 请求体（POST /api/v1/vms）。 */
export type CreateVmRequest = VmSpec;

// ============================================================================
// 共享（os-api::handlers::share::ShareInfo）
// ============================================================================

/** 文件共享（API 返回的 DTO，与 os-protocols::Share 字段对齐）。 */
export interface Share {
  id: string;
  name: string;
  /** 协议（smb / nfs / webdav / ...，lowercase 字符串）。 */
  protocol: string;
  path: string;
  read_only: boolean;
  enabled: boolean;
}

/** NFS 导出条目。 */
export interface NfsExport {
  path: string;
  client: string;
  options: string;
}

// ============================================================================
// 用户（os-security::User）
// ============================================================================

/** 用户角色（lowercase 字符串；后端 Role 枚举序列化）。 */
export type Role = string;

/** 用户。 */
export interface User {
  id: string;
  name: string;
  roles: Role[];
  enabled: boolean;
  /** 创建时间（RFC3339）。 */
  created_at: string;
}

// ============================================================================
// 节点（os-core::NodeInfo）
// ============================================================================

/** 节点角色（NodeRole，snake_case）。 */
export type NodeRole = 'leader' | 'follower' | 'peer' | 'standalone';

/** 联邦节点信息。 */
export interface NodeInfo {
  node_id: string;
  role: NodeRole;
  version: string;
  arch: string;
  endpoints: string[];
  health: Health;
}

// ============================================================================
// 系统状态（os-api::handlers::system /status 聚合）
// ============================================================================

/** CPU 虚拟化能力详查（VirtCheckResult，best-effort 容错字段）。
 *
 * 网关 `/status` 的 `cpu_virt` 子对象直接复用 os-compute::virt_check::VirtCheckResult
 * 的 serde 序列化字段（snake_case），枚举为 externally-tagged 表示：
 *   - cpu_vendor: "Intel" | "Amd" | { "Unknown": "<vendor_id>" }
 *   - nested_virt: { "Supported": true|false } | "Unknown"
 * 前端使用 @/composables/useFormat 的 cpuVendorLabel / nestedVirtLabel 归一化展示。
 */
export interface CpuVirt {
  /** CPU 厂商（serde 标签枚举：字符串 Intel/Amd 或 {"Unknown": "..."} 对象）。 */
  cpu_vendor?: string | { Unknown: string };
  /** CPU 是否具备硬件虚拟化标志位（vmx/svm）。 */
  cpu_has_virt_flags?: boolean;
  /** /dev/kvm 是否存在（KVM 模块加载后由 udev 创建）。 */
  kvm_device_present?: boolean;
  /** kvm/kvm_intel/kvm_amd 模块是否加载（读 /proc/modules）。 */
  kvm_module_loaded?: boolean;
  /** 嵌套虚拟化状态（serde 标签枚举：{"Supported": bool} 或 "Unknown"）。 */
  nested_virt?: { Supported: boolean } | 'Unknown';
  // —— 向前兼容字段（旧静态页契约；新网关返回 VirtCheckResult 全字段）——
  has_vmx?: boolean;
  has_svm?: boolean;
  /** KVM 模块是否加载（/dev/kvm 是否存在）。 */
  has_kvm?: boolean;
  /** /dev/kvm 是否可访问。 */
  kvm_available?: boolean;
  /** 综合判定（虚拟化是否可用）。 */
  is_usable?: boolean;
  /** 用户友好的中文诊断文本（to_user_diagnostic）。 */
  diagnostic?: string;
  /** 检测失败时的错误说明（字段存在则检测失败）。 */
  error?: string;
}

/** GET /status 聚合响应（对齐 os_mobile::SystemStatus 客户端契约）。 */
export interface SystemStatus {
  hostname: string;
  version: string;
  capacity: Capacity;
  health: Health;
  node_count: number;
  /** 向前兼容扩展字段（SystemStatus 反序列化时忽略）。 */
  cpu_virt?: CpuVirt;
  /** 网关进程 uptime（秒）。 */
  uptime?: number;
}

/** GET /api/v1/version 响应。 */
export interface VersionInfo {
  name: string;
  version: string;
}

// ============================================================================
// IM（聊天：群组 / 对话 / 消息 / 节点）
// ============================================================================

/** IM 群组/对话类型。 */
export type ImConversationKind = 'group' | 'direct';

/** IM 群组/对话。 */
export interface ImGroup {
  id: string;
  name: string;
  kind: ImConversationKind;
  members?: string[];
  /** 最后一条消息时间（RFC3339，可空）。 */
  last_activity?: string;
}

/** 兼容别名：群组即一个会话。 */
export type ImConversation = ImGroup;

/** IM 消息类型。 */
export type ImMessageType = 'text' | 'file' | 'image' | 'system';

/** IM 消息。 */
export interface ImMessage {
  id: string;
  /** 所属会话/群组 id。 */
  conversation_id: string;
  /** 发送者 id（自己发送的消息，前端按 isOwn 判定对齐）。 */
  sender_id: string;
  /** 发送者显示名。 */
  sender_name?: string;
  /** 消息文本内容。 */
  content: string;
  /** 消息类型（text/file/image/system）。 */
  msg_type?: ImMessageType;
  /** 文件/图片 URL（msg_type=file/image 时有值）。 */
  file_url?: string;
  /** 被回复消息 id。 */
  reply_to?: string;
  /** 创建时间（RFC3339）。 */
  created_at: string;
  /** 已读用户 id 列表。 */
  read_by?: string[];
}

/** IM 已连接节点/对端连接状态。 */
export type ImPeerStatus = 'online' | 'offline' | 'connecting';

/** IM 已连接节点/对端。 */
export interface ImPeer {
  id: string;
  /** 节点名（可空）。 */
  name?: string;
  /** 远端地址，形如 host:port 或 tcp://ip:port。 */
  endpoint: string;
  /** 兼容老字段（部分响应仍用 addr）。 */
  addr?: string;
  status: ImPeerStatus;
  /** 最近一次握手时间（RFC3339，可空）。 */
  last_seen?: string;
}

/** IM 大厅成员（60s 心跳窗口内活跃 = 在线）。 */
export interface ImLobbyMember {
  user_id: string;
  display_name?: string;
  /** 最近一次心跳（RFC3339，可空）。 */
  last_seen?: string;
  /** 加入时间（RFC3339，可空）。 */
  joined_at?: string;
  /** 派生字段：last_seen 距今 < 60s。 */
  online: boolean;
}

/** IM 大厅信息（GET /api/v1/im/lobby）。 */
export interface ImLobbyInfo {
  /** 恒为 'lobby'。 */
  id: string;
  name: string;
  member_count: number;
  online_count: number;
  /** 最近一条消息（可空）。 */
  last_message?: ImMessage | null;
}
