<script setup lang="ts">
// =============================================================================
// Storage.vue —— 存储管理（池 / 数据集 / 快照 三 tab）
//
// 创建池：飞牛 fnOS 风格「左右分栏向导」三步式
//   Step 1 选硬盘：左 55% 可用硬盘卡片（勾选），右 45% 动态摘要 + 可用模式提示
//   Step 2 配置存储：左 存储模式卡片（按选盘数动态过滤 RAID 类型 + 容量计算），
//                    右 高级选项（文件系统 + 池名 + 可选 L2ARC/ZIL 折叠区）
//   Step 3 确认创建：摘要 + 红字警告 + 输入池名确认（防误操作）→ POST /api/v1/pools
//
// 删除池（2026-08-30，TrueNAS Export/Destroy 式）：池详情 → 红色「删除池…」→
//   确认对话框（成员盘 + 容量摘要 + 输入池名确认 + 两种模式单选）→
//   DELETE /api/v1/pools/:name?wipe=。删除后的盘两条出路均有 UI 承接：
//   - 保留标签（zpool export）→ 「可导入的存储池」横幅（可一键导入恢复）；
//   - 彻底擦除（destroy + wipefs）→ 盘变空白，出现在创建池向导可选列表。
//
// 容量计算 calcCapacity 为纯函数（可单测）。
// 最终仍调现有 POST /api/v1/pools，body 组装成 VdevSpec[]，不改后端契约。
// =============================================================================
import { computed, onMounted, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import DataTable from '@/components/DataTable.vue';
import type { Column } from '@/components/data-table';
import HealthBadge from '@/components/HealthBadge.vue';
import CapacityBar from '@/components/CapacityBar.vue';
import FileBrowser from '@/components/FileBrowser.vue';
import { del, endpoints, isZfsUnavailable, post } from '@/api/client';
import type { ImportablePool } from '@/api/client';
import { formatBytes, formatDateTime, ratioPct } from '@/utils/format';
import type {
  Dataset,
  DiskInfo,
  DiskPartitions,
  Pool,
  Snapshot,
  VdevSpec,
} from '@/api/types';

// i18n：本页新增文案走 vue-i18n（四语言）；存量文案保持硬编码中文，
// 待整页迁移时统一收口（与 ApiGateway/ModelHub 等页的渐进式迁移同款）。
const { t } = useI18n();

type Tab = 'pools' | 'datasets' | 'snapshots' | 'files';

const tabs: { key: Tab; label: string }[] = [
  { key: 'pools', label: '存储池' },
  { key: 'datasets', label: '数据集' },
  { key: 'snapshots', label: '快照' },
  { key: 'files', label: '文件浏览' },
];

const activeTab = ref<Tab>('pools');

// —— 数据 ——
const pools = ref<Pool[]>([]);
const datasets = ref<Dataset[]>([]);
const snapshots = ref<Snapshot[]>([]);

const poolsLoading = ref(false);
const datasetsLoading = ref(false);
const snapshotsLoading = ref(false);

const poolsError = ref('');
const datasetsError = ref('');
const snapshotsError = ref('');

/**
 * 本节点未安装 ZFS 工具（后端探测 PATH 无 zpool/zfs，读端点返回
 * `{<key>:[], zfs_available:false}` 降级空态）。不是错误——不显示红幅，
 * 改为低调信息条 + 禁用创建池/数据集/快照入口；文件浏览不受影响。
 */
const zfsUnavailable = ref(false);

// 数据集按池筛选
const poolFilter = ref<string>('');
const poolFilterOptions = computed(() =>
  pools.value.map((p) => p.name || p.id).filter(Boolean),
);

/** 任一列表正在加载（顶部刷新按钮态）。 */
const anyLoading = computed(
  () => poolsLoading.value || datasetsLoading.value || snapshotsLoading.value,
);

// —— 加载函数 ——
async function loadPools(): Promise<void> {
  poolsLoading.value = true;
  poolsError.value = '';
  try {
    const v = await endpoints.pools();
    if (isZfsUnavailable(v)) {
      // ZFS 工具缺失：后端 200 空态降级——不报错，置降级标志
      zfsUnavailable.value = true;
      pools.value = [];
      return;
    }
    pools.value = Array.isArray(v) ? v : [v as Pool];
  } catch (e) {
    poolsError.value = e instanceof Error ? e.message : String(e);
    pools.value = [];
  } finally {
    poolsLoading.value = false;
  }
}

async function loadDatasets(): Promise<void> {
  datasetsLoading.value = true;
  datasetsError.value = '';
  try {
    const v = await endpoints.datasets(poolFilter.value || undefined);
    if (isZfsUnavailable(v)) {
      zfsUnavailable.value = true;
      datasets.value = [];
      return;
    }
    datasets.value = v;
  } catch (e) {
    datasetsError.value = e instanceof Error ? e.message : String(e);
    datasets.value = [];
  } finally {
    datasetsLoading.value = false;
  }
}

async function loadSnapshots(): Promise<void> {
  snapshotsLoading.value = true;
  snapshotsError.value = '';
  try {
    const v = await endpoints.snapshots();
    if (isZfsUnavailable(v)) {
      zfsUnavailable.value = true;
      snapshots.value = [];
      return;
    }
    snapshots.value = v;
  } catch (e) {
    snapshotsError.value = e instanceof Error ? e.message : String(e);
    snapshots.value = [];
  } finally {
    snapshotsLoading.value = false;
  }
}

/** 顶部「刷新」按钮：按当前 tab 刷新对应列表。 */
function refresh(): void {
  if (activeTab.value === 'pools') {
    void loadPools();
    void loadImportable(); // 可导入池横幅随之刷新
  } else if (activeTab.value === 'datasets') void loadDatasets();
  else if (activeTab.value === 'snapshots') void loadSnapshots();
  else browserRef.value?.refresh(); // 文件浏览：组件自刷新
}

// 切换 tab 时按需加载（文件浏览 tab 由 FileBrowser 挂载时自加载，无需处理）
function switchTab(tab: Tab): void {
  activeTab.value = tab;
  if (tab === 'pools') void loadPools();
  else if (tab === 'datasets') void loadDatasets();
  else if (tab === 'snapshots') void loadSnapshots();
}

// =============================================================================
// 文件浏览（只读）：FileBrowser 以 /tank 为根，查看池内容
//   —— 用 v-if 而非 v-show：离开 tab 即卸载组件，停掉其 5s 轮询
// =============================================================================
const browserRef = ref<InstanceType<typeof FileBrowser> | null>(null);

// 池筛选变化：重新拉数据集
function onFilterChange(): void {
  void loadDatasets();
}

// =============================================================================
// 飞牛风格「创建池」向导（三步左右分栏）
// =============================================================================

/** RAID 模式（前端展示用，最终映射为后端 VdevSpec 的 kind/vdev 组合）。 */
type RaidMode = 'single' | 'raid0' | 'raid1' | 'raid5' | 'raid10';
type Filesystem = 'zfs' | 'ext4' | 'btrfs';
type WizardStep = 1 | 2 | 3;

interface CapacityResult {
  /** 所有选中盘容量之和（原始总容量）。 */
  total: number;
  /** 该 RAID 模式下可用容量（字节）。 */
  usable: number;
  /** 可容忍故障盘数。 */
  faultTolerance: number;
}

/** 存储模式元信息（标题 / 说明）。 */
const MODE_META: Record<RaidMode, { label: string; desc: string }> = {
  single: { label: '单盘', desc: '直接使用单块硬盘，无冗余。' },
  raid0: { label: 'RAID0 条带', desc: '多盘条带，容量=总和，任一盘故障即全部丢失。' },
  raid1: { label: 'RAID1 镜像', desc: '两盘互为镜像，容量=单盘，可容忍 1 块故障。' },
  raid5: { label: 'RAID5', desc: '单奇偶校验，容量=(N-1)×最小盘，可容忍 1 块故障。' },
  raid10: { label: 'RAID10', desc: '镜像+条带，容量=(N/2)×最小盘，每镜像组可容忍 1 块。' },
};

/**
 * 根据选中硬盘容量列表 + RAID 模式 计算可用容量（纯函数，可单测）。
 *   single : usable=min,        fault=0
 *   raid0  : usable=sum,        fault=0
 *   raid1  : usable=min,        fault=1（≥2 盘）
 *   raid5  : usable=(n-1)*min,  fault=1（≥3 盘）
 *   raid10 : usable=(n/2)*min,  fault=1（≥4 偶数，每组 1）
 */
function calcCapacity(diskSizes: number[], mode: RaidMode): CapacityResult {
  const n = diskSizes.length;
  if (n === 0) return { total: 0, usable: 0, faultTolerance: 0 };
  const total = diskSizes.reduce((a, b) => a + b, 0);
  const min = Math.min(...diskSizes);
  switch (mode) {
    case 'single':
      return { total, usable: min, faultTolerance: 0 };
    case 'raid0':
      return { total, usable: total, faultTolerance: 0 };
    case 'raid1':
      return { total, usable: min, faultTolerance: n >= 2 ? 1 : 0 };
    case 'raid5':
      return { total, usable: (n - 1) * min, faultTolerance: n >= 3 ? 1 : 0 };
    case 'raid10':
      return { total, usable: Math.floor(n / 2) * min, faultTolerance: n >= 4 ? 1 : 0 };
  }
}

/** 按选盘数返回可选存储模式（顺序即默认推荐顺序）。 */
function modesForCount(n: number): RaidMode[] {
  if (n < 1) return [];
  const modes: RaidMode[] = ['single'];
  if (n >= 2) modes.push('raid0', 'raid1');
  if (n >= 3) modes.push('raid5');
  if (n >= 4 && n % 2 === 0) modes.push('raid10');
  return modes;
}

/** 由设备名/型号推断 SSD/HDD（后端 DiskInfo 无 rotational 字段，启发式）。 */
function diskType(d: DiskInfo): 'SSD' | 'HDD' {
  const s = `${d.name} ${d.model}`.toUpperCase();
  if (s.includes('NVME') || s.includes('SSD')) return 'SSD';
  return 'HDD';
}

/** 设备路径 → 裸设备名（/dev/nvme1n1 → nvme1n1；磁盘端点 :name 用裸名）。 */
function bareName(name: string): string {
  return name.replace(/^\/dev\//, '');
}

/**
 * 磁盘是否需要「初始化」（wipefs -a）才能加入新池。
 *
 * 白名单（永不提示初始化，2026-08-30 定稿）：
 * - 系统盘：后端 detect_disks 已把挂 /、/boot*、[SWAP] 的盘整盘过滤，不会出现在列表；
 * - 活跃池成员（member_of，如 tank 的 sda）——删除该池后才能重新初始化；
 * - 可导入池成员（zfs_pool_hint，如已导出的 nvme 池）——数据没丢，导入即恢复。
 * 只有「无主残留签名盘」才进入初始化流程。
 */
function needsInit(d: DiskInfo): boolean {
  if (d.member_of || d.zfs_pool_hint) return false;
  return !!d.has_partitions;
}

/** 活跃池成员：所属池名（灰显标注「池内成员」，不可初始化/不可选入新池）。 */
function memberOfPool(d: DiskInfo): string | null {
  return d.member_of || null;
}

/** 可导入池成员：所属池名（蓝标「属于可导入池」+ 导入按钮）。 */
function importableHint(d: DiskInfo): string | null {
  return d.zfs_pool_hint || null;
}

/** 是否可选入新池（数据/缓存/日志通用）：需初始化盘、活跃池成员、可导入池成员均不可选。 */
function selectableForNewPool(d: DiskInfo): boolean {
  return !needsInit(d) && !d.member_of && !d.zfs_pool_hint;
}

/** 需初始化盘的签名摘要（如 "BitLocker / gpt"；未知时给通用文案）。 */
function signaturesText(d: DiskInfo): string {
  const sigs = (d.signatures || []).filter(Boolean);
  return sigs.length ? sigs.join(' / ') : '未知签名';
}

/** 故障容忍的人类可读描述。 */
function faultText(mode: RaidMode, fault: number): string {
  if (mode === 'raid10') return '每镜像组可容忍 1 块盘故障';
  if (fault === 0) return '无冗余：任一盘故障即数据丢失';
  return `可容忍 ${fault} 块盘故障`;
}

// —— 向导状态 ——
const showCreatePool = ref(false);
const wizardStep = ref<WizardStep>(1);
const createPoolSubmitting = ref(false);
const createPoolMsg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);

const availableDisks = ref<DiskInfo[]>([]);
const disksLoading = ref(false);
const disksError = ref('');

// 第一步选中的「数据盘」集合
const selectedDiskNames = ref<Set<string>>(new Set());
// 高级：可选缓存盘（L2ARC，纯读缓存）/ 日志盘（ZIL/SLOG）
const l2arcDiskNames = ref<Set<string>>(new Set());
const logDiskNames = ref<Set<string>>(new Set());

// 第二步配置
const raidMode = ref<RaidMode>('single');
const filesystem = ref<Filesystem>('zfs');
const poolName = ref('');
const showAdvanced = ref(false);

// 第三步确认（输入的名称必须与 poolName 完全一致才能创建）
const confirmName = ref('');

// —— 派生 ——
/** 已选数据盘列表（按名称稳定排序）。 */
const selectedDisks = computed<DiskInfo[]>(() => {
  const names = Array.from(selectedDiskNames.value).sort();
  return names
    .map((n) => availableDisks.value.find((d) => d.name === n))
    .filter((d): d is DiskInfo => !!d);
});

/** 已选数据盘容量列表（字节）。 */
const selectedSizes = computed<number[]>(() =>
  selectedDisks.value.map((d) => d.size_bytes),
);

/** 当前可选存储模式列表（按选盘数动态过滤）。 */
const availableModes = computed<RaidMode[]>(() =>
  modesForCount(selectedDiskNames.value.size),
);

/** 当前选中模式的容量预估。 */
const capacity = computed<CapacityResult>(() =>
  calcCapacity(selectedSizes.value, raidMode.value),
);

/** 各可用模式的容量预估（左侧模式卡片展示）。 */
const modeCards = computed(() =>
  availableModes.value.map((m) => ({
    mode: m,
    meta: MODE_META[m],
    cap: calcCapacity(selectedSizes.value, m),
  })),
);

/** 数据盘可选列表：完全空闲的盘（用户 08-30：被使用的盘不可见——属池/可导入池/需初始化盘全部隐藏，只显示干净盘）。 */
const dataDiskOptions = computed<DiskInfo[]>(() =>
  availableDisks.value
    .filter(
      (d) =>
        !l2arcDiskNames.value.has(d.name) &&
        !logDiskNames.value.has(d.name) &&
        // 用户定调（08-30）：被使用的盘不可见——属池/可导入池/需初始化的盘
        // 全部从新建池候选里隐藏，只显示完全空闲的盘（灰显禁选改为直接不渲染）
        selectableForNewPool(d),
    )
    .slice()
    .sort((a, b) => a.name.localeCompare(b.name)),
);

/** 数据盘里可直接勾选的盘（排除需初始化/池成员盘——全选/摘要只统计这些）。 */
const usableDataDisks = computed<DiskInfo[]>(() =>
  dataDiskOptions.value.filter((d) => selectableForNewPool(d)),
);

/** 缓存盘（L2ARC）可选列表：未被数据/日志选中，且可直选（已初始化、非池成员）。 */
const l2arcDiskOptions = computed<DiskInfo[]>(() =>
  availableDisks.value.filter(
    (d) =>
      selectableForNewPool(d) &&
      !selectedDiskNames.value.has(d.name) &&
      !logDiskNames.value.has(d.name),
  ),
);

/** 日志盘（ZIL）可选列表：未被数据/缓存选中，且可直选（已初始化、非池成员）。 */
const logDiskOptions = computed<DiskInfo[]>(() =>
  availableDisks.value.filter(
    (d) =>
      selectableForNewPool(d) &&
      !selectedDiskNames.value.has(d.name) &&
      !l2arcDiskNames.value.has(d.name),
  ),
);

/** 已选数据盘最大/最小单盘容量。 */
const selectedMinMax = computed<{ min: number; max: number }>(() => {
  const sizes = selectedSizes.value;
  if (!sizes.length) return { min: 0, max: 0 };
  return { min: Math.min(...sizes), max: Math.max(...sizes) };
});

/** 已选数据盘（+缓存/日志盘）是否全部已初始化（无残留分区/签名）。 */
const allSelectedUsable = computed(() => {
  const names = [
    ...selectedDiskNames.value,
    ...l2arcDiskNames.value,
    ...logDiskNames.value,
  ];
  return names.every((n) => {
    const d = availableDisks.value.find((x) => x.name === n);
    return !d || !needsInit(d);
  });
});

/** 第三步可创建：确认名 === 池名 且 非空 且 所有选中盘已初始化。 */
const canCreate = computed(
  () =>
    !!poolName.value.trim() &&
    confirmName.value.trim() === poolName.value.trim() &&
    allSelectedUsable.value &&
    !createPoolSubmitting.value,
);

/** 第二步可进入第三步：已选模式（隐含）且池名非空且所有选中盘已初始化。 */
const canGoStep3 = computed(
  () => !!poolName.value.trim() && allSelectedUsable.value,
);

// 选盘数变化后，若当前模式不再可用，则回退到推荐模式（首个）
watch(availableModes, (modes) => {
  if (modes.length && !modes.includes(raidMode.value)) {
    raidMode.value = modes[0];
  }
});

// —— 交互 ——
function toggleDisk(name: string): void {
  if (createPoolSubmitting.value) return;
  // 需初始化盘（残留分区/签名）与池成员盘（活跃池/可导入池）都不可选——
  // 前者必须先初始化，后者属于别的池（导入或删池后另行处理）
  const d = availableDisks.value.find((x) => x.name === name);
  if (d && !selectableForNewPool(d)) return;
  const next = new Set(selectedDiskNames.value);
  if (next.has(name)) next.delete(name);
  else next.add(name);
  selectedDiskNames.value = next;
}

function toggleL2arc(name: string): void {
  if (createPoolSubmitting.value) return;
  const next = new Set(l2arcDiskNames.value);
  if (next.has(name)) next.delete(name);
  else next.add(name);
  l2arcDiskNames.value = next;
}

function toggleLog(name: string): void {
  if (createPoolSubmitting.value) return;
  const next = new Set(logDiskNames.value);
  if (next.has(name)) next.delete(name);
  else next.add(name);
  logDiskNames.value = next;
}

/** 全选/反选数据盘（第一步工具按钮）——只作用于已初始化的可选盘。 */
function selectAllDataDisks(): void {
  if (createPoolSubmitting.value) return;
  if (selectedDiskNames.value.size === usableDataDisks.value.length) {
    selectedDiskNames.value = new Set();
  } else {
    selectedDiskNames.value = new Set(usableDataDisks.value.map((d) => d.name));
  }
}

/** 加载可用磁盘列表（打开对话框时调用）。 */
async function loadDisks(): Promise<void> {
  disksLoading.value = true;
  disksError.value = '';
  try {
    availableDisks.value = await endpoints.disks();
  } catch (e) {
    disksError.value = e instanceof Error ? e.message : String(e);
    availableDisks.value = [];
  } finally {
    disksLoading.value = false;
  }
}

// =============================================================================
// 可导入池识别与导入（2026-08-30：zpool import 探测是只读列表，绝不真导）
// =============================================================================

const importablePools = ref<ImportablePool[]>([]);
const importableLoading = ref(false);
/** 正在导入的池名（'' = 空闲；防重复提交）。 */
const importingPool = ref('');
/** 导入操作的结果提示（横幅 + 磁盘卡片共用）。 */
const importMsg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);

/** 探测可导入池（GET /disks/importable；任何失败降级为空，不打扰用户）。 */
async function loadImportable(): Promise<void> {
  importableLoading.value = true;
  try {
    const v = await endpoints.importablePools();
    importablePools.value = v.importable || [];
  } catch {
    importablePools.value = [];
  } finally {
    importableLoading.value = false;
  }
}

/**
 * 导入一个已导出的 ZFS 池（confirm → POST /disks/import → 刷新）。
 * - 横幅场景（opts.refreshDisks 缺省）：导入后刷新 pools + importable；
 * - 向导磁盘卡片场景（refreshDisks: true）：额外刷新磁盘列表（盘从
 *   「可导入」变为活跃池成员）。
 * 失败把后端错误体（含 zpool 原始 stderr）展示在 importMsg。
 */
async function importPool(
  name: string,
  opts: { refreshDisks?: boolean } = {},
): Promise<void> {
  if (!name || importingPool.value) return;
  const confirmed = window.confirm(
    `确认导入存储池「${name}」？\n\n导入将挂载该池的全部数据集（数据不会丢失）。` +
      '若与现有池重名，导入会失败并提示原因。',
  );
  if (!confirmed) return;
  importingPool.value = name;
  importMsg.value = { kind: 'info', text: `正在导入存储池 ${name}…` };
  try {
    await endpoints.importPool(name);
    importMsg.value = { kind: 'ok', text: `存储池 ${name} 导入成功，数据已恢复可用` };
    await loadPools();
    await loadImportable();
    if (opts.refreshDisks && showCreatePool.value) await loadDisks();
  } catch (e) {
    importMsg.value = {
      kind: 'err',
      text: `导入 ${name} 失败：` + (e instanceof Error ? e.message : String(e)),
    };
  } finally {
    importingPool.value = '';
  }
}

// =============================================================================
// 删除池（TrueNAS Export/Destroy 式，2026-08-30）：输入池名确认 + 两种模式
//   - export（默认）：仅删除池，磁盘 ZFS 标签保留 → 进「可导入的存储池」横幅，
//     可一键导入恢复（zpool export；destroy 后无参 zpool import 不列出，故选 export）
//   - wipe：彻底擦除（zpool destroy + 逐盘 wipefs -a）→ 盘变空白，可直接建新池
// =============================================================================

type PoolDeleteMode = 'export' | 'wipe';

/** 待删除的池（null = 对话框关闭）。 */
const deleteTarget = ref<Pool | null>(null);
/** 删除模式单选（默认保留标签）。 */
const deleteMode = ref<PoolDeleteMode>('export');
/** 输入的池名确认（TrueNAS 式防误删：必须与池名完全一致）。 */
const deleteConfirmName = ref('');
const deleteSubmitting = ref(false);
const deleteMsg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);
/** 删除池操作的结果提示（池面板顶部展示，与对话框内 deleteMsg 分离）。 */
const poolDeleteMsg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);

/** 池成员盘列表（详情/确认对话框展示用；实际处置以后端 zpool status 抓取为准）。 */
function poolMemberDisks(p: Pool): string[] {
  const names: string[] = [];
  for (const v of p.vdevs || []) {
    for (const d of v.disks || []) names.push(d);
  }
  return names;
}

function openDeletePool(p: Pool): void {
  deleteTarget.value = p;
  deleteMode.value = 'export';
  deleteConfirmName.value = '';
  deleteMsg.value = null;
}

function closeDeletePool(): void {
  if (deleteSubmitting.value) return;
  deleteTarget.value = null;
}

/** 确认按钮可用：输入的确认名 === 池名（trim 后）。 */
const canConfirmDelete = computed(() => {
  const t = deleteTarget.value;
  return (
    !!t &&
    deleteConfirmName.value.trim() === (t.name || t.id).trim() &&
    !deleteSubmitting.value
  );
});

/**
 * 执行删除：DELETE /pools/:name?wipe= → 刷新 pools / importable（export 的池
 * 会出现在可导入横幅——删池后盘的两条出路都有 UI 承接）。
 * wipe 的盘变空白 → 创建池向导打开时会重新拉 disks，无需在此刷新。
 */
async function submitDeletePool(): Promise<void> {
  const t = deleteTarget.value;
  if (!t || !canConfirmDelete.value) return;
  const name = t.name || t.id;
  const wipe = deleteMode.value === 'wipe';
  deleteSubmitting.value = true;
  deleteMsg.value = {
    kind: 'info',
    text: wipe ? '正在删除池并逐盘擦除成员盘…' : '正在删除池（保留数据标签）…',
  };
  try {
    const resp = await endpoints.poolDelete(name, wipe);
    deleteTarget.value = null;
    // 刷新：池列表少一行；可导入池横幅认领 export 保留标签的盘
    await loadPools();
    await loadImportable();
    // 数据集/快照可能含该池条目，静默刷新（失败不阻断成功提示）
    void loadDatasets();
    void loadSnapshots();
    let text = wipe
      ? `存储池 ${name} 已删除并彻底擦除 ${resp.wiped_disks.length} 块成员盘` +
        `（${resp.wiped_disks.map(bareName).join(' / ')}）——盘已空白，可创建新池`
      : `存储池 ${name} 已删除（磁盘标签保留）——可在下方「可导入的存储池」重新导入`;
    if (resp.warning) text += `；注意：${resp.warning}`;
    poolDeleteMsg.value = { kind: 'ok', text };
    if (wipe) importMsg.value = null; // 清掉旧导入提示，避免误导（wipe 后无可导入池）
  } catch (e) {
    deleteMsg.value = {
      kind: 'err',
      text: '删除失败：' + (e instanceof Error ? e.message : String(e)),
    };
  } finally {
    deleteSubmitting.value = false;
  }
}

// =============================================================================
// 磁盘初始化（残留分区/签名 → 两步确认 → wipefs -a）
// =============================================================================

// —— 分区详情（[查看详情]）——
/** 当前展开分区详情的磁盘名（'' = 全收起）。 */
const detailDiskName = ref<string>('');
/** 分区详情缓存（按磁盘名；初始化成功后清对应键）。 */
const partitionsCache = ref<Record<string, DiskPartitions>>({});
/** 正在加载分区详情的磁盘名。 */
const partitionsLoading = ref<string>('');

/** 展开/收起某盘的分区详情（首次展开时拉取 GET /disks/:name/partitions）。 */
async function viewPartitions(d: DiskInfo): Promise<void> {
  const key = d.name;
  if (detailDiskName.value === key) {
    detailDiskName.value = '';
    return;
  }
  detailDiskName.value = key;
  if (partitionsCache.value[key] || partitionsLoading.value === key) return;
  partitionsLoading.value = key;
  try {
    partitionsCache.value = {
      ...partitionsCache.value,
      [key]: await endpoints.diskPartitions(bareName(key)),
    };
  } catch (e) {
    // 失败不阻断——在缓存槽放一个空降级结构（带 warning），界面可重试（收起再展开）
    partitionsCache.value = {
      ...partitionsCache.value,
      [key]: {
        disk: bareName(key),
        has_partitions: needsInit(d),
        signatures: d.signatures || [],
        partitions: [],
        warning: e instanceof Error ? e.message : String(e),
      },
    };
  } finally {
    partitionsLoading.value = '';
  }
}

// —— 初始化两步确认（[⚡ 初始化磁盘]）——
/** 待初始化的磁盘（null = 对话框关闭）。 */
const initTarget = ref<DiskInfo | null>(null);
/** 确认步骤：1=红色警告确认 → 2=输入磁盘名确认。 */
const initStep = ref<1 | 2>(1);
/** 第二步输入的磁盘名（必须与裸名完全一致才能执行）。 */
const initConfirmName = ref('');
const initSubmitting = ref(false);
const initMsg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);

function openInit(d: DiskInfo): void {
  // 白名单兜底：活跃池成员 / 可导入池成员永不进入初始化流程（防误 wipefs 毁池）
  if (d.member_of || d.zfs_pool_hint) return;
  initTarget.value = d;
  initStep.value = 1;
  initConfirmName.value = '';
  initMsg.value = null;
}

function closeInit(): void {
  if (initSubmitting.value) return;
  initTarget.value = null;
}

/** 第二步可执行：输入的确认名 === 磁盘裸名。 */
const canConfirmInit = computed(() => {
  const t = initTarget.value;
  return (
    !!t &&
    initConfirmName.value.trim() === bareName(t.name) &&
    !initSubmitting.value
  );
});

/** 执行初始化：POST /disks/:name/initialize → 成功后刷新磁盘列表（盘变可用）。 */
async function submitInit(): Promise<void> {
  const t = initTarget.value;
  if (!t || !canConfirmInit.value) return;
  initSubmitting.value = true;
  initMsg.value = { kind: 'info', text: '正在初始化（wipefs -a 清除签名）…' };
  try {
    const resp = await endpoints.initializeDisk(bareName(t.name));
    initTarget.value = null;
    // 刷新磁盘列表：该盘 has_partitions 应变为 false（FSTYPE 为空 → 可选）
    await loadDisks();
    const wiped = (resp.wiped || []).join(' / ');
    createPoolMsg.value = {
      kind: 'ok',
      text: `${t.name} 初始化完成${wiped ? `（已清除 ${wiped}）` : ''}，现在可加入存储池`,
    };
    // 清理该盘的分区详情缓存（数据已过期）
    const next = { ...partitionsCache.value };
    delete next[t.name];
    partitionsCache.value = next;
    detailDiskName.value = '';
  } catch (e) {
    initMsg.value = {
      kind: 'err',
      text: '初始化失败：' + (e instanceof Error ? e.message : String(e)),
    };
  } finally {
    initSubmitting.value = false;
  }
}

function openCreatePool(): void {
  wizardStep.value = 1;
  createPoolMsg.value = null;
  selectedDiskNames.value = new Set();
  l2arcDiskNames.value = new Set();
  logDiskNames.value = new Set();
  raidMode.value = 'single';
  filesystem.value = 'zfs';
  poolName.value = '';
  confirmName.value = '';
  showAdvanced.value = false;
  availableDisks.value = [];
  disksError.value = '';
  showCreatePool.value = true;
  void loadDisks();
}

function closeCreatePool(): void {
  if (createPoolSubmitting.value) return;
  showCreatePool.value = false;
}

function goNext(): void {
  if (wizardStep.value === 1) {
    if (!selectedDiskNames.value.size) return;
    // 所有选中盘必须已初始化（未初始化盘不可勾选，此处兜底）
    if (!allSelectedUsable.value) return;
    // 进入第二步时，若当前模式不在可用集合内，回退到推荐
    const modes = availableModes.value;
    if (!modes.includes(raidMode.value)) raidMode.value = modes[0] ?? 'single';
    wizardStep.value = 2;
  } else if (wizardStep.value === 2) {
    if (!canGoStep3.value) return;
    wizardStep.value = 3;
  }
}

function goPrev(): void {
  if (wizardStep.value === 2) wizardStep.value = 1;
  else if (wizardStep.value === 3) wizardStep.value = 2;
}

/** 根据 RAID 模式 + 已选数据盘组装后端 VdevSpec[]（data vdev 主体）。 */
function buildDataVdevs(mode: RaidMode, dataDisks: string[]): VdevSpec[] {
  const vdevs: VdevSpec[] = [];
  const disks = dataDisks.slice().sort();
  switch (mode) {
    case 'single':
      if (disks[0]) vdevs.push({ kind: 'disk', disks: [disks[0]], role: 'data' });
      break;
    case 'raid0':
      // 多个单盘 vdev 组成条带池（ZFS 里 = 无冗余 RAID0）
      disks.forEach((d) => vdevs.push({ kind: 'disk', disks: [d], role: 'data' }));
      break;
    case 'raid1':
      if (disks.length) vdevs.push({ kind: 'mirror', disks, role: 'data' });
      break;
    case 'raid5':
      if (disks.length) vdevs.push({ kind: 'raidz1', disks, role: 'data' });
      break;
    case 'raid10':
      // 两两配对成 mirror vdev
      for (let i = 0; i + 1 < disks.length; i += 2) {
        vdevs.push({ kind: 'mirror', disks: [disks[i], disks[i + 1]], role: 'data' });
      }
      break;
  }
  return vdevs;
}

async function submitCreatePool(): Promise<void> {
  const name = poolName.value.trim();
  if (!name) {
    createPoolMsg.value = { kind: 'err', text: '请填写池名' };
    return;
  }
  if (confirmName.value.trim() !== name) {
    createPoolMsg.value = { kind: 'err', text: '确认名称与池名不一致' };
    return;
  }
  const dataDisks = Array.from(selectedDiskNames.value);
  if (!dataDisks.length) {
    createPoolMsg.value = { kind: 'err', text: '请至少选择一块数据盘' };
    return;
  }

  // 组装 vdevs：数据 vdev（按模式） + 可选日志盘（mirror≥2 / disk=1） + 可选缓存盘（disk）
  const vdevs: VdevSpec[] = buildDataVdevs(raidMode.value, dataDisks);
  const logDisks = Array.from(logDiskNames.value);
  const l2arcDisks = Array.from(l2arcDiskNames.value);
  if (logDisks.length) {
    const logKind = logDisks.length >= 2 ? 'mirror' : 'disk';
    vdevs.push({ kind: logKind, disks: logDisks, role: 'log' });
  }
  if (l2arcDisks.length) {
    vdevs.push({ kind: 'disk', disks: l2arcDisks, role: 'cache' });
  }

  createPoolSubmitting.value = true;
  createPoolMsg.value = { kind: 'info', text: '创建中…' };
  try {
    await endpoints.createPool({ name, vdevs });
    showCreatePool.value = false;
    await loadPools();
  } catch (e) {
    createPoolMsg.value = {
      kind: 'err',
      text: '创建失败：' + (e instanceof Error ? e.message : String(e)),
    };
  } finally {
    createPoolSubmitting.value = false;
  }
}

// =============================================================================
// 池详情对话框（显示 vdev / 磁盘布局）
// =============================================================================
const detailPool = ref<Pool | null>(null);

function openPoolDetail(p: Pool): void {
  detailPool.value = p;
}
function closePoolDetail(): void {
  detailPool.value = null;
}

// =============================================================================
// 创建数据集对话框
// =============================================================================
const showCreateDataset = ref(false);
const createDsForm = ref<{ name: string; pool: string; quotaGiB: string }>({
  name: '',
  pool: '',
  quotaGiB: '',
});
const createDsSubmitting = ref(false);
const createDsMsg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);

function openCreateDataset(): void {
  createDsForm.value = { name: '', pool: poolFilter.value || '', quotaGiB: '' };
  createDsMsg.value = null;
  // 若池列表为空，先加载一次，填充下拉
  if (!pools.value.length) void loadPools();
  showCreateDataset.value = true;
}

function closeCreateDataset(): void {
  if (createDsSubmitting.value) return;
  showCreateDataset.value = false;
}

async function submitCreateDataset(): Promise<void> {
  const nameFrag = createDsForm.value.name.trim();
  const pool = createDsForm.value.pool.trim();
  const quotaRaw = createDsForm.value.quotaGiB.trim();

  if (!nameFrag) {
    createDsMsg.value = { kind: 'err', text: '请填写数据集名称' };
    return;
  }
  if (!pool) {
    createDsMsg.value = { kind: 'err', text: '请选择所属池' };
    return;
  }

  // 名称只允许为单层片段（不含 '/'，避免与拼接产生歧义）
  if (nameFrag.includes('/')) {
    createDsMsg.value = {
      kind: 'err',
      text: '名称请只填最后一段（不含 "/"），所属池已单独选择',
    };
    return;
  }

  const fullName = `${pool}/${nameFrag}`;

  // 可选配额：GiB → 字节
  let refquota: number | undefined;
  if (quotaRaw) {
    const gib = Number(quotaRaw);
    if (!Number.isFinite(gib) || gib <= 0) {
      createDsMsg.value = { kind: 'err', text: '配额需为正数（GiB）' };
      return;
    }
    refquota = Math.round(gib * 1024 * 1024 * 1024);
  }

  createDsSubmitting.value = true;
  createDsMsg.value = { kind: 'info', text: '创建中…' };
  try {
    await post('/api/v1/datasets', {
      name: fullName,
      options: refquota ? { quota: { refquota } } : {},
    });
    showCreateDataset.value = false;
    await loadDatasets();
  } catch (e) {
    createDsMsg.value = {
      kind: 'err',
      text: '创建失败：' + (e instanceof Error ? e.message : String(e)),
    };
  } finally {
    createDsSubmitting.value = false;
  }
}

// =============================================================================
// 创建快照对话框
// =============================================================================
const showCreateSnapshot = ref(false);
const createSnapForm = ref<{ dataset: string; name: string }>({ dataset: '', name: '' });
const createSnapSubmitting = ref(false);
const createSnapMsg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);

function openCreateSnapshot(): void {
  createSnapForm.value = { dataset: '', name: '' };
  createSnapMsg.value = null;
  // 若数据集列表为空，先加载一次，填充下拉
  if (!datasets.value.length) void loadDatasets();
  showCreateSnapshot.value = true;
}

function closeCreateSnapshot(): void {
  if (createSnapSubmitting.value) return;
  showCreateSnapshot.value = false;
}

async function submitCreateSnapshot(): Promise<void> {
  const dataset = createSnapForm.value.dataset.trim();
  const name = createSnapForm.value.name.trim();
  if (!dataset) {
    createSnapMsg.value = { kind: 'err', text: '请选择数据集' };
    return;
  }
  if (!name) {
    createSnapMsg.value = { kind: 'err', text: '请填写快照名' };
    return;
  }

  createSnapSubmitting.value = true;
  createSnapMsg.value = { kind: 'info', text: '创建中…' };
  try {
    // 后端当前路由表未声明 POST /api/v1/snapshots；按 RESTful 约定尝试，
    // 失败（404/405）在 catch 给出可读提示。
    await post('/api/v1/snapshots', { dataset, name });
    showCreateSnapshot.value = false;
    await loadSnapshots();
  } catch (e) {
    const m = e instanceof Error ? e.message : String(e);
    const friendly = /404|405|未匹配|not found|method not allowed/i.test(m)
      ? '后端暂不支持创建快照（未实现 POST /api/v1/snapshots）'
      : '创建失败：' + m;
    createSnapMsg.value = { kind: 'err', text: friendly };
  } finally {
    createSnapSubmitting.value = false;
  }
}

// =============================================================================
// 删除快照（后端未实现，失败友好降级）
// =============================================================================
const deletingSnapshotId = ref<string>('');

async function deleteSnapshot(s: Snapshot): Promise<void> {
  const id = s.id;
  if (!id) return;
  if (!window.confirm(`确定删除快照「${id}」？该操作不可撤销。`)) return;
  deletingSnapshotId.value = id;
  try {
    await del(`/api/v1/snapshots/${encodeURIComponent(id)}`);
    await loadSnapshots();
  } catch (e) {
    const m = e instanceof Error ? e.message : String(e);
    const friendly = /404|405|未匹配|not found|method not allowed/i.test(m)
      ? '后端暂不支持删除快照（未实现 DELETE /api/v1/snapshots/<id>）'
      : '删除失败：' + m;
    snapshotsError.value = friendly;
  } finally {
    deletingSnapshotId.value = '';
  }
}

// =============================================================================
// 表格列定义
// =============================================================================
const poolColumns: Column<Pool>[] = [
  { key: 'name', title: '池名', accessor: (p) => p.name || p.id },
  { key: 'health', title: '健康', width: '100px' },
  { key: 'capacity', title: '容量（已用 / 总量 · 使用率）', width: '360px' },
  { key: 'vdevs', title: 'vdev 数', width: '90px', align: 'right', sortable: true,
    accessor: (p) => (p.vdevs || []).length },
  { key: 'actions', title: '操作', width: '110px', align: 'right' },
];

const datasetColumns: Column<Dataset>[] = [
  { key: 'name', title: '名称', accessor: (d) => d.name || d.id },
  { key: 'pool', title: '所属池', width: '120px', sortable: true },
  { key: 'used', title: '已用', width: '120px', align: 'right', sortable: true,
    accessor: (d) => d.used_bytes },
  { key: 'avail', title: '可用', width: '120px', align: 'right', sortable: true,
    accessor: (d) => d.avail_bytes },
  { key: 'mounted', title: '挂载', width: '100px' },
  { key: 'encryption', title: '加密', width: '110px' },
];

const snapshotColumns: Column<Snapshot>[] = [
  { key: 'id', title: '名称' },
  { key: 'dataset', title: '所属数据集', width: '180px' },
  { key: 'created', title: '创建时间', width: '180px', sortable: true,
    accessor: (s) => s.created },
  { key: 'used', title: '大小', width: '120px', align: 'right', sortable: true,
    accessor: (s) => s.used_bytes },
  { key: 'actions', title: '操作', width: '110px', align: 'right' },
];

// 加密状态徽章映射（locked=unhealthy, unlocked=healthy, off=unknown）
function encryptionMeta(enc: string): { health: 'healthy' | 'unhealthy' | 'unknown'; label: string } {
  switch ((enc || '').toLowerCase()) {
    case 'unlocked':
      return { health: 'healthy', label: '已解锁' };
    case 'locked':
      return { health: 'unhealthy', label: '已锁定' };
    default:
      return { health: 'unknown', label: '未加密' };
  }
}

/** 池详情里使用的使用率百分比（0~100 整数）。 */
function poolPct(p: Pool): number {
  const t = Number(p.capacity?.total_bytes ?? 0);
  if (!t) return 0;
  return ratioPct((Number(p.capacity?.used_bytes ?? 0)) / t);
}

onMounted(() => {
  void loadPools();
  void loadImportable();
});
</script>

<template>
  <div class="storage-page">
    <div class="page-head">
      <h2 class="page-title">存储管理</h2>
      <button class="btn btn-small" :disabled="anyLoading" @click="refresh">
        <span class="spin" :class="{ spinning: anyLoading }" aria-hidden="true">↻</span>
        刷新
      </button>
    </div>

    <!-- Tab 栏 -->
    <div class="tabs">
      <button
        v-for="t in tabs"
        :key="t.key"
        type="button"
        class="tab-btn"
        :class="{ active: activeTab === t.key }"
        @click="switchTab(t.key)"
      >
        {{ t.label }}
      </button>
    </div>

    <!-- 存储池 -->
    <section v-show="activeTab === 'pools'" class="panel">
      <div class="panel-head">
        <h3>存储池</h3>
        <button
          class="btn btn-primary"
          :disabled="zfsUnavailable"
          :title="zfsUnavailable ? t('storageZfs.createDisabledTip') : undefined"
          @click="openCreatePool"
        >
          ＋ 创建池
        </button>
      </div>

      <!-- ZFS 工具缺失：低调信息条（非红幅错误）——自动检查出没有 ZFS 就不显示相关功能 -->
      <div v-if="zfsUnavailable" class="info-box" role="note">
        {{ t('storageZfs.unavailableBanner') }}
      </div>

      <!-- 可导入池横幅：检测到已导出/未导入的 ZFS 池时展示（一键导入恢复数据） -->
      <div v-if="importablePools.length" class="card import-banner">
        <div class="import-banner-head">
          💾 检测到 {{ importablePools.length }} 个可导入的存储池
          <span v-if="importableLoading" class="hint">（探测中…）</span>
        </div>
        <div
          v-for="p in importablePools"
          :key="p.id || p.name"
          class="import-banner-row"
        >
          <span class="mono import-banner-name">{{ p.name }}</span>
          <span :class="['pill', p.state === 'ONLINE' ? 'pill-ok' : 'pill-muted']">
            {{ p.state || 'ONLINE' }}
          </span>
          <button
            type="button"
            class="btn btn-small btn-primary"
            :disabled="!!importingPool"
            @click="importPool(p.name)"
          >
            {{ importingPool === p.name ? '导入中…' : '导入此池' }}
          </button>
        </div>
        <p v-if="importMsg" :class="['import-banner-msg', `msg-${importMsg.kind}`]">
          {{ importMsg.text }}
        </p>
      </div>

      <div v-if="poolsError" class="error-box">加载失败：{{ poolsError }}</div>
      <!-- 删除池结果提示（export 成功 → 可导入横幅随之出现；wipe 成功 → 盘可建新池） -->
      <p v-if="poolDeleteMsg" :class="['form-msg', `is-${poolDeleteMsg.kind}`]">
        {{ poolDeleteMsg.text }}
      </p>

      <div class="card card-table">
        <DataTable
          :columns="poolColumns"
          :rows="pools"
          :loading="poolsLoading"
          :empty-text="
            zfsUnavailable ? t('storageZfs.poolsEmpty') : '暂无存储池，点击右上角「创建池」添加。'
          "
        >
          <template #cell-health="{ row }">
            <HealthBadge :health="row.health" />
          </template>
          <template #cell-capacity="{ row }">
            <CapacityBar :capacity="row.capacity" :show-text="true" />
          </template>
          <template #cell-actions="{ row }">
            <button class="btn btn-small" @click.stop="openPoolDetail(row)">详情</button>
          </template>
        </DataTable>
      </div>
    </section>

    <!-- 数据集 -->
    <section v-show="activeTab === 'datasets'" class="panel">
      <div class="panel-head">
        <h3>数据集</h3>
        <div class="head-actions">
          <div class="filter-group">
            <label for="ds-pool-filter">按池筛选：</label>
            <select id="ds-pool-filter" v-model="poolFilter" @change="onFilterChange">
              <option value="">（全部）</option>
              <option v-for="name in poolFilterOptions" :key="name" :value="name">{{ name }}</option>
            </select>
          </div>
          <button
            class="btn btn-primary"
            :disabled="zfsUnavailable"
            :title="zfsUnavailable ? t('storageZfs.createDisabledTip') : undefined"
            @click="openCreateDataset"
          >
            ＋ 创建数据集
          </button>
        </div>
      </div>

      <div v-if="zfsUnavailable" class="info-box" role="note">
        {{ t('storageZfs.unavailableBanner') }}
      </div>

      <div v-if="datasetsError" class="error-box">加载失败：{{ datasetsError }}</div>

      <div class="card card-table">
        <DataTable
          :columns="datasetColumns"
          :rows="datasets"
          :loading="datasetsLoading"
          :empty-text="zfsUnavailable ? t('storageZfs.datasetsEmpty') : '暂无数据集。'"
        >
          <template #cell-used="{ row }">{{ formatBytes(row.used_bytes) }}</template>
          <template #cell-avail="{ row }">{{ formatBytes(row.avail_bytes) }}</template>
          <template #cell-mounted="{ row }">
            <span :class="['pill', row.mounted ? 'pill-ok' : 'pill-muted']">
              {{ row.mounted ? '已挂载' : '未挂载' }}
            </span>
          </template>
          <template #cell-encryption="{ row }">
            <span :class="['pill', `pill-${encryptionMeta(row.encryption).health}`]">
              {{ encryptionMeta(row.encryption).label }}
            </span>
          </template>
        </DataTable>
      </div>
    </section>

    <!-- 快照 -->
    <section v-show="activeTab === 'snapshots'" class="panel">
      <div class="panel-head">
        <h3>快照</h3>
        <button
          class="btn btn-primary"
          :disabled="zfsUnavailable"
          :title="zfsUnavailable ? t('storageZfs.createDisabledTip') : undefined"
          @click="openCreateSnapshot"
        >
          ＋ 创建快照
        </button>
      </div>

      <div v-if="zfsUnavailable" class="info-box" role="note">
        {{ t('storageZfs.unavailableBanner') }}
      </div>

      <div v-if="snapshotsError" class="error-box">{{ snapshotsError }}</div>

      <div class="card card-table">
        <DataTable
          :columns="snapshotColumns"
          :rows="snapshots"
          :loading="snapshotsLoading"
          :empty-text="zfsUnavailable ? t('storageZfs.snapshotsEmpty') : '暂无快照。'"
        >
          <template #cell-created="{ row }">{{ formatDateTime(row.created) }}</template>
          <template #cell-used="{ row }">{{ formatBytes(row.used_bytes) }}</template>
          <template #cell-actions="{ row }">
            <button
              class="btn btn-small btn-danger"
              :disabled="deletingSnapshotId === row.id"
              @click.stop="deleteSnapshot(row)"
            >
              {{ deletingSnapshotId === row.id ? '删除中…' : '删除' }}
            </button>
          </template>
        </DataTable>
      </div>
    </section>

    <!-- 文件浏览（只读）：ZFS 池 /tank 内容；v-if 离开 tab 即卸载、停轮询 -->
    <section v-if="activeTab === 'files'" class="panel">
      <div class="panel-head">
        <h3>文件浏览</h3>
      </div>
      <p class="hint">
        浏览 ZFS 池 <code>/tank</code> 的内容，只读视图；
        新建/删除/重命名等管理操作请前往「文件管理」页。
      </p>
      <FileBrowser ref="browserRef" root="/tank" readonly />
    </section>

    <!-- ============ 创建池向导（飞牛风格三步左右分栏） ============ -->
    <div v-if="showCreatePool" class="modal-backdrop" @click.self="closeCreatePool">
      <div class="modal modal-wizard" role="dialog" aria-modal="true" aria-labelledby="create-pool-title">
        <div class="modal-head">
          <h3 id="create-pool-title">创建存储池</h3>
          <button class="modal-close" type="button" :disabled="createPoolSubmitting" @click="closeCreatePool">
            ×
          </button>
        </div>

        <!-- 顶部 stepper -->
        <div class="stepper">
          <div
            v-for="s in [{n:1,t:'选择硬盘'},{n:2,t:'配置存储'},{n:3,t:'确认创建'}]"
            :key="s.n"
            class="step"
            :class="{ active: wizardStep === s.n, done: wizardStep > s.n }"
          >
            <span class="step-num">{{ wizardStep > s.n ? '✓' : s.n }}</span>
            <span class="step-text">{{ s.t }}</span>
          </div>
        </div>

        <div class="modal-body">
          <!-- ============ Step 1：选择硬盘（左右分栏） ============ -->
          <div v-if="wizardStep === 1" class="wizard-split">
            <!-- 左：可用硬盘列表 -->
            <div class="split-left">
              <div class="split-head">
                <span class="split-title">可用硬盘</span>
                <button
                  type="button"
                  class="btn btn-small"
                  :disabled="createPoolSubmitting || !usableDataDisks.length"
                  @click="selectAllDataDisks"
                >
                  {{ selectedDiskNames.size === usableDataDisks.length && usableDataDisks.length
                    ? '清空' : '全选' }}
                </button>
              </div>
              <p v-if="disksLoading" class="hint">正在检测本机磁盘…</p>
              <p v-else-if="disksError" class="hint hint-warn">磁盘检测失败：{{ disksError }}</p>
              <p v-else-if="!dataDiskOptions.length" class="hint hint-warn">
                    没有可用的数据盘（系统盘/ZFS 成员已自动过滤，或已被缓存/日志区选中）。
                  </p>
              <div v-else class="disk-grid">
                <div v-for="d in dataDiskOptions" :key="d.name" class="disk-item">
                  <!-- 主卡片：需初始化/池成员盘禁选（灰显 + 徽标） -->
                  <label
                    class="disk-card"
                    :class="{
                      checked: selectedDiskNames.has(d.name),
                      'needs-init': needsInit(d),
                      'is-member': !!d.member_of,
                      'is-importable': !!d.zfs_pool_hint,
                    }"
                  >
                    <input
                      type="checkbox"
                      :checked="selectedDiskNames.has(d.name)"
                      :disabled="createPoolSubmitting || !selectableForNewPool(d)"
                      @change="toggleDisk(d.name)"
                    />
                    <div class="disk-card-main">
                      <div class="disk-card-row">
                        <span class="disk-name mono">{{ d.name }}</span>
                        <span :class="['type-pill', `type-${diskType(d).toLowerCase()}`]">{{ diskType(d) }}</span>
                        <!-- 徽标：池内成员（灰）→ 可导入池（蓝）→ 需初始化（红） -->
                        <span v-if="memberOfPool(d)" class="init-flag member-flag">
                          池内成员: {{ memberOfPool(d) }}
                        </span>
                        <span v-else-if="importableHint(d)" class="init-flag hint-flag">
                          属于可导入池: {{ importableHint(d) }}
                        </span>
                        <span v-else-if="needsInit(d)" class="init-flag">需先初始化</span>
                      </div>
                      <div class="disk-card-sub">
                        <span class="disk-size">{{ formatBytes(d.size_bytes) }}</span>
                        <span v-if="d.model" class="disk-model">· {{ d.model }}</span>
                      </div>
                    </div>
                  </label>

                  <!-- 活跃池成员（灰色）：删除该池后才能重新初始化——不给初始化按钮 -->
                  <div v-if="memberOfPool(d)" class="init-warn member-warn">
                    <div class="init-warn-text">
                      🔒 {{ d.name }} 属于活跃池
                      <strong>{{ memberOfPool(d) }}</strong>
                      ，删除该池后才能重新初始化
                    </div>
                    <div class="init-warn-actions">
                      <button
                        type="button"
                        class="btn btn-small"
                        :disabled="partitionsLoading === d.name"
                        @click="viewPartitions(d)"
                      >
                        {{ detailDiskName === d.name ? '收起详情' : '查看详情' }}
                      </button>
                    </div>
                  </div>

                  <!-- 可导入池成员（蓝色）：数据完好，导入即恢复——绝不提示 wipefs -->
                  <div v-else-if="importableHint(d)" class="init-warn importable-warn">
                    <div class="init-warn-text">
                      💾 {{ d.name }} 属于已导出的存储池
                      <strong>{{ importableHint(d) }}</strong>
                      ，数据完好——可导入恢复，无需初始化
                    </div>
                    <div class="init-warn-actions">
                      <button
                        type="button"
                        class="btn btn-small"
                        :disabled="partitionsLoading === d.name"
                        @click="viewPartitions(d)"
                      >
                        {{ detailDiskName === d.name ? '收起详情' : '查看详情' }}
                      </button>
                      <button
                        type="button"
                        class="btn btn-small btn-primary"
                        :disabled="!!importingPool"
                        @click="importPool(importableHint(d)!, { refreshDisks: true })"
                      >
                        {{ importingPool === importableHint(d) ? '导入中…' : '导入此池' }}
                      </button>
                    </div>
                  </div>

                  <!-- 无主残留签名（黄色）：查看详情 / 初始化磁盘 -->
                  <div v-else-if="needsInit(d)" class="init-warn">
                    <div class="init-warn-text">
                      ⚠️ {{ d.name }} 上检测到已有分区/签名（{{ signaturesText(d) }}），
                      需先初始化才能加入存储池
                    </div>
                    <div class="init-warn-actions">
                      <button
                        type="button"
                        class="btn btn-small"
                        :disabled="partitionsLoading === d.name"
                        @click="viewPartitions(d)"
                      >
                        {{ detailDiskName === d.name ? '收起详情' : '查看详情' }}
                      </button>
                      <button
                        type="button"
                        class="btn btn-small btn-warn"
                        :disabled="initSubmitting"
                        @click="openInit(d)"
                      >
                        ⚡ 初始化磁盘
                      </button>
                    </div>
                  </div>

                  <!-- 分区详情面板（GET /disks/:name/partitions） -->
                  <div v-if="detailDiskName === d.name" class="part-detail">
                    <p v-if="partitionsLoading === d.name" class="hint">正在读取分区表…</p>
                    <template v-else-if="partitionsCache[d.name]">
                      <p v-if="partitionsCache[d.name].warning" class="hint hint-warn">
                        {{ partitionsCache[d.name].warning }}
                      </p>
                      <p class="hint">
                        签名：
                        <code>{{
                          partitionsCache[d.name].signatures.length
                            ? partitionsCache[d.name].signatures.join(' / ')
                            : '无'
                        }}</code>
                      </p>
                      <div
                        v-if="partitionsCache[d.name].partitions.length"
                        class="part-list"
                      >
                        <div
                          v-for="p in partitionsCache[d.name].partitions"
                          :key="p.name"
                          class="part-row"
                        >
                          <span class="mono">{{ p.name }}</span>
                          <span>{{ p.size }}</span>
                          <span>{{ p.fstype || '—' }}</span>
                          <span class="part-label">{{ p.label || '' }}</span>
                        </div>
                      </div>
                      <p v-else class="hint">无分区记录（可能是整盘文件系统签名）。</p>
                    </template>
                  </div>
                </div>
              </div>
            </div>

            <!-- 右：动态摘要 -->
            <div class="split-right">
              <div class="split-head">
                <span class="split-title">已选摘要</span>
              </div>
              <div class="summary-card">
                <div class="summary-row big">
                  <span class="summary-label">已选硬盘</span>
                  <span class="summary-val">{{ selectedDiskNames.size }} 块</span>
                </div>
                <div class="summary-row">
                  <span class="summary-label">总容量</span>
                  <span class="summary-val mono">{{ formatBytes(capacity.total) }}</span>
                </div>
                <div class="summary-row">
                  <span class="summary-label">最小单盘</span>
                  <span class="summary-val mono">{{ formatBytes(selectedMinMax.min) }}</span>
                </div>
                <div class="summary-row">
                  <span class="summary-label">最大单盘</span>
                  <span class="summary-val mono">{{ formatBytes(selectedMinMax.max) }}</span>
                </div>
              </div>

              <div v-if="selectedDiskNames.size" class="mode-hints">
                <p class="hint">可用存储模式：</p>
                <ul class="hint-list">
                  <li v-for="m in availableModes" :key="m">
                    <code>{{ MODE_META[m].label }}</code> · {{ MODE_META[m].desc }}
                  </li>
                </ul>
              </div>
              <p v-else class="hint hint-warn">请在左侧勾选硬盘。</p>
            </div>
          </div>

          <!-- ============ Step 2：配置存储（左右分栏） ============ -->
          <div v-else-if="wizardStep === 2" class="wizard-split">
            <!-- 左：存储模式卡片 -->
            <div class="split-left">
              <div class="split-head">
                <span class="split-title">存储模式</span>
                <span class="hint">基于 {{ selectedDiskNames.size }} 块硬盘</span>
              </div>
              <div class="mode-grid">
                <button
                  v-for="c in modeCards"
                  :key="c.mode"
                  type="button"
                  class="mode-card"
                  :class="{ selected: raidMode === c.mode }"
                  :disabled="createPoolSubmitting"
                  @click="raidMode = c.mode"
                >
                  <div class="mode-card-head">
                    <span class="mode-name">{{ c.meta.label }}</span>
                    <span v-if="raidMode === c.mode" class="mode-check">✓</span>
                  </div>
                  <div class="mode-cap">
                    可用 <strong class="mono">{{ formatBytes(c.cap.usable) }}</strong>
                  </div>
                  <p class="mode-desc">{{ c.meta.desc }}</p>
                  <p class="mode-fault">{{ faultText(c.mode, c.cap.faultTolerance) }}</p>
                </button>
              </div>
            </div>

            <!-- 右：高级选项 -->
            <div class="split-right">
              <div class="split-head">
                <span class="split-title">高级选项</span>
              </div>
              <div class="field">
                <label>文件系统</label>
                <div class="radio-row">
                  <label class="radio-item" :class="{ on: filesystem === 'zfs' }">
                    <input v-model="filesystem" type="radio" value="zfs" />
                    <span>ZFS</span>
                  </label>
                  <label class="radio-item disabled">
                    <input type="radio" value="ext4" disabled />
                    <span>ext4 <em>即将支持</em></span>
                  </label>
                  <label class="radio-item disabled">
                    <input type="radio" value="btrfs" disabled />
                    <span>Btrfs <em>即将支持</em></span>
                  </label>
                </div>
                <p class="hint">当前后端仅支持 ZFS；ext4/Btrfs 为占位。</p>
              </div>

              <div class="field">
                <label for="pool-name">池名称</label>
                <input
                  id="pool-name"
                  v-model="poolName"
                  type="text"
                  placeholder="例如 tank"
                  :disabled="createPoolSubmitting"
                />
              </div>

              <!-- 高级：可选缓存盘 / 日志盘 -->
              <div class="advanced-toggle">
                <button type="button" class="link-btn" @click="showAdvanced = !showAdvanced">
                  {{ showAdvanced ? '▾' : '▸' }} 加速盘（可选 L2ARC / ZIL）
                </button>
              </div>
              <div v-if="showAdvanced" class="advanced-box">
                <div class="field">
                  <label>缓存盘 L2ARC（纯读缓存，可多盘）</label>
                  <p v-if="!l2arcDiskOptions.length" class="hint hint-warn">没有可选缓存盘。</p>
                  <div v-else class="mini-disk-list">
                    <label
                      v-for="d in l2arcDiskOptions"
                      :key="d.name"
                      class="mini-disk-row"
                      :class="{ checked: l2arcDiskNames.has(d.name) }"
                    >
                      <input
                        type="checkbox"
                        :checked="l2arcDiskNames.has(d.name)"
                        :disabled="createPoolSubmitting"
                        @change="toggleL2arc(d.name)"
                      />
                      <span class="mono">{{ d.name }}</span>
                      <span class="disk-meta">{{ formatBytes(d.size_bytes) }}</span>
                    </label>
                  </div>
                </div>
                <div class="field">
                  <label>日志盘 ZIL（同步写，≥2 块做 mirror）</label>
                  <p v-if="!logDiskOptions.length" class="hint hint-warn">没有可选日志盘。</p>
                  <div v-else class="mini-disk-list">
                    <label
                      v-for="d in logDiskOptions"
                      :key="d.name"
                      class="mini-disk-row"
                      :class="{ checked: logDiskNames.has(d.name) }"
                    >
                      <input
                        type="checkbox"
                        :checked="logDiskNames.has(d.name)"
                        :disabled="createPoolSubmitting"
                        @change="toggleLog(d.name)"
                      />
                      <span class="mono">{{ d.name }}</span>
                      <span class="disk-meta">{{ formatBytes(d.size_bytes) }}</span>
                    </label>
                  </div>
                </div>
              </div>
            </div>
          </div>

          <!-- ============ Step 3：确认创建 ============ -->
          <div v-else class="confirm-wrap">
            <div class="confirm-card">
              <div class="confirm-section">
                <div class="confirm-label">已选硬盘（{{ selectedDiskNames.size }}）</div>
                <div class="chip-row">
                  <span v-for="d in selectedDisks" :key="d.name" class="disk-chip">
                    <span class="mono">{{ d.name }}</span>
                    <span class="chip-meta">{{ formatBytes(d.size_bytes) }}</span>
                  </span>
                </div>
              </div>
              <div class="confirm-grid">
                <div class="confirm-row">
                  <span class="confirm-label">存储模式</span>
                  <span>{{ MODE_META[raidMode].label }}</span>
                </div>
                <div class="confirm-row">
                  <span class="confirm-label">文件系统</span>
                  <span>ZFS</span>
                </div>
                <div class="confirm-row">
                  <span class="confirm-label">池名</span>
                  <span class="mono">{{ poolName || '（未填）' }}</span>
                </div>
                <div class="confirm-row">
                  <span class="confirm-label">预估总容量</span>
                  <span class="mono">{{ formatBytes(capacity.total) }}</span>
                </div>
                <div class="confirm-row">
                  <span class="confirm-label">可用容量</span>
                  <span class="mono strong">{{ formatBytes(capacity.usable) }}</span>
                </div>
                <div class="confirm-row">
                  <span class="confirm-label">冗余</span>
                  <span>{{ faultText(raidMode, capacity.faultTolerance) }}</span>
                </div>
              </div>
              <div v-if="logDiskNames.size || l2arcDiskNames.size" class="confirm-section">
                <div class="confirm-label">加速盘</div>
                <p v-if="logDiskNames.size" class="hint">
                  ZIL：<code>{{ Array.from(logDiskNames).join(', ') }}</code>
                </p>
                <p v-if="l2arcDiskNames.size" class="hint">
                  L2ARC：<code>{{ Array.from(l2arcDiskNames).join(', ') }}</code>
                </p>
              </div>
            </div>

            <div class="warn-box">
              创建将格式化所选硬盘，<strong>所有数据将丢失且不可恢复</strong>。
            </div>

            <div class="field">
              <label for="confirm-name">请输入池名「<code>{{ poolName || '...' }}</code>」以确认</label>
              <input
                id="confirm-name"
                v-model="confirmName"
                type="text"
                :placeholder="poolName"
                :disabled="createPoolSubmitting"
              />
            </div>
          </div>

          <p v-if="createPoolMsg" :class="['form-msg', `is-${createPoolMsg.kind}`]">{{ createPoolMsg.text }}</p>

          <!-- 底部导航 -->
          <div class="form-actions">
            <button
              v-if="wizardStep > 1"
              type="button"
              class="btn"
              :disabled="createPoolSubmitting"
              @click="goPrev"
            >
              上一步
            </button>
            <button
              v-else
              type="button"
              class="btn"
              :disabled="createPoolSubmitting"
              @click="closeCreatePool"
            >
              取消
            </button>
            <button
              v-if="wizardStep < 3"
              type="button"
              class="btn btn-primary"
              :disabled="createPoolSubmitting
                || (wizardStep === 1 && !selectedDiskNames.size)
                || (wizardStep === 2 && !canGoStep3)"
              @click="goNext"
            >
              下一步
            </button>
            <button
              v-else
              type="button"
              class="btn btn-danger strong-btn"
              :disabled="!canCreate"
              @click="submitCreatePool"
            >
              {{ createPoolSubmitting ? '创建中…' : '创建' }}
            </button>
          </div>
        </div>
      </div>
    </div>

    <!-- ============ 磁盘初始化两步确认（红色警告 → 输入磁盘名确认 → wipefs -a） ============ -->
    <div v-if="initTarget" class="modal-backdrop" @click.self="closeInit">
      <div class="modal modal-init" role="dialog" aria-modal="true" aria-labelledby="init-disk-title">
        <div class="modal-head">
          <h3 id="init-disk-title">初始化磁盘 · {{ initTarget.name }}</h3>
          <button class="modal-close" type="button" :disabled="initSubmitting" @click="closeInit">
            ×
          </button>
        </div>
        <div class="modal-body">
          <!-- 第一步：红色警告（将清除所有数据） -->
          <template v-if="initStep === 1">
            <div class="warn-box">
              将清除 <strong class="mono">{{ initTarget.name }}</strong> 上的
              <strong>全部分区表与文件系统签名</strong>（{{ signaturesText(initTarget) }}）。
              <strong>该磁盘上所有数据将丢失且不可恢复！</strong>
            </div>
            <p class="hint">
              若该盘曾属于其他系统（如 Windows BitLocker / 其他 NAS），请先确认不再需要其数据。
            </p>
          </template>
          <!-- 第二步：输入磁盘名确认 -->
          <template v-else>
            <div class="warn-box">
              最后一步：请输入磁盘名 <code>{{ bareName(initTarget.name) }}</code> 以确认擦除。
            </div>
            <div class="field">
              <label for="init-confirm-name">磁盘名确认</label>
              <input
                id="init-confirm-name"
                v-model="initConfirmName"
                type="text"
                :placeholder="bareName(initTarget.name)"
                :disabled="initSubmitting"
              />
            </div>
          </template>
          <p v-if="initMsg" :class="['form-msg', `is-${initMsg.kind}`]">{{ initMsg.text }}</p>
          <div class="form-actions">
            <button
              v-if="initStep === 2"
              type="button"
              class="btn"
              :disabled="initSubmitting"
              @click="initStep = 1"
            >
              上一步
            </button>
            <button v-else type="button" class="btn" :disabled="initSubmitting" @click="closeInit">
              取消
            </button>
            <button
              v-if="initStep === 1"
              type="button"
              class="btn btn-danger strong-btn"
              :disabled="initSubmitting"
              @click="initStep = 2"
            >
              我已知晓风险，继续
            </button>
            <button
              v-else
              type="button"
              class="btn btn-danger strong-btn"
              :disabled="!canConfirmInit"
              @click="submitInit"
            >
              {{ initSubmitting ? '初始化中…' : '确认初始化' }}
            </button>
          </div>
        </div>
      </div>
    </div>

    <!-- ============ 删除池确认（TrueNAS Export/Destroy 式：输入池名 + 两种模式单选） ============ -->
    <div v-if="deleteTarget" class="modal-backdrop" @click.self="closeDeletePool">
      <div class="modal modal-init" role="dialog" aria-modal="true" aria-labelledby="del-pool-title">
        <div class="modal-head">
          <h3 id="del-pool-title">删除存储池 · {{ deleteTarget.name || deleteTarget.id }}</h3>
          <button class="modal-close" type="button" :disabled="deleteSubmitting" @click="closeDeletePool">
            ×
          </button>
        </div>
        <div class="modal-body">
          <!-- 摘要：成员盘 + 容量 -->
          <div class="warn-box">
            即将删除存储池 <strong class="mono">{{ deleteTarget.name || deleteTarget.id }}</strong>：
            容量
            <span class="mono">{{ formatBytes(deleteTarget.capacity?.total_bytes) }}</span>（已用
            <span class="mono">{{ formatBytes(deleteTarget.capacity?.used_bytes) }}</span>），
            成员盘
            <span v-if="poolMemberDisks(deleteTarget).length" class="mono">
              {{ poolMemberDisks(deleteTarget).join(' / ') }}
            </span>
            <span v-else class="muted">（未知——后端会在删除前从 zpool status 抓取）</span>。
            池内<strong>全部数据集与快照将随所选模式处置</strong>。
          </div>

          <!-- 两种删除模式单选（默认保留标签，可重新导入） -->
          <div class="del-modes" role="radiogroup" aria-label="删除模式">
            <label class="del-mode" :class="{ checked: deleteMode === 'export' }">
              <input
                type="radio"
                name="del-mode"
                value="export"
                :disabled="deleteSubmitting"
                v-model="deleteMode"
              />
              <div class="del-mode-body">
                <strong>仅删除池（保留数据标签，可重新导入）</strong>
                <p class="hint">
                  zpool export：池从系统移除、数据集卸载，但磁盘上的 ZFS 标签保留——
                  删除后出现在「可导入的存储池」横幅，可随时一键导入恢复数据。
                </p>
              </div>
            </label>
            <label class="del-mode is-danger" :class="{ checked: deleteMode === 'wipe' }">
              <input
                type="radio"
                name="del-mode"
                value="wipe"
                :disabled="deleteSubmitting"
                v-model="deleteMode"
              />
              <div class="del-mode-body">
                <strong class="danger-text">彻底擦除（wipefs 成员盘，数据不可恢复）</strong>
                <p class="hint danger-text">
                  zpool destroy + 对每块成员盘执行 wipefs：上述成员盘将被清空全部分区表与
                  签名，<strong>数据全部丢失且无法恢复</strong>；盘变为完全空白，可直接用于创建新池。
                </p>
              </div>
            </label>
          </div>

          <!-- 输入池名确认（TrueNAS 式防误删） -->
          <div class="field">
            <label for="del-confirm-name">
              请输入池名「<code>{{ deleteTarget.name || deleteTarget.id }}</code>」以确认删除
            </label>
            <input
              id="del-confirm-name"
              v-model="deleteConfirmName"
              type="text"
              :placeholder="deleteTarget.name || deleteTarget.id"
              :disabled="deleteSubmitting"
            />
          </div>

          <p v-if="deleteMsg" :class="['form-msg', `is-${deleteMsg.kind}`]">{{ deleteMsg.text }}</p>

          <div class="form-actions">
            <button type="button" class="btn" :disabled="deleteSubmitting" @click="closeDeletePool">
              取消
            </button>
            <button
              type="button"
              class="btn btn-danger strong-btn"
              :disabled="!canConfirmDelete"
              @click="submitDeletePool"
            >
              {{
                deleteSubmitting
                  ? '删除中…'
                  : deleteMode === 'wipe'
                    ? '删除池并彻底擦除'
                    : '删除池'
              }}
            </button>
          </div>
        </div>
      </div>
    </div>

    <!-- ============ 创建数据集对话框 ============ -->
    <div v-if="showCreateDataset" class="modal-backdrop" @click.self="closeCreateDataset">
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="create-ds-title">
        <div class="modal-head">
          <h3 id="create-ds-title">创建数据集</h3>
          <button class="modal-close" type="button" :disabled="createDsSubmitting" @click="closeCreateDataset">
            ×
          </button>
        </div>
        <form class="modal-body" @submit.prevent="submitCreateDataset">
          <div class="field">
            <label for="ds-name">名称（最后一段，不含 "/"）</label>
            <input
              id="ds-name"
              v-model="createDsForm.name"
              type="text"
              placeholder="例如 media"
              :disabled="createDsSubmitting"
            />
            <p class="hint">最终路径为 <code>&lt;所属池&gt;/&lt;名称&gt;</code>。</p>
          </div>
          <div class="field">
            <label for="ds-pool">所属池</label>
            <select id="ds-pool" v-model="createDsForm.pool" :disabled="createDsSubmitting">
              <option value="" disabled>请选择池…</option>
              <option v-for="name in poolFilterOptions" :key="name" :value="name">{{ name }}</option>
            </select>
            <p v-if="!poolFilterOptions.length" class="hint hint-warn">尚未加载到任何池，请先创建池。</p>
          </div>
          <div class="field">
            <label for="ds-quota">配额（GiB，可选）</label>
            <input
              id="ds-quota"
              v-model="createDsForm.quotaGiB"
              type="number"
              min="1"
              step="1"
              placeholder="留空 = 不设配额"
              :disabled="createDsSubmitting"
            />
          </div>
          <p v-if="createDsMsg" :class="['form-msg', `is-${createDsMsg.kind}`]">{{ createDsMsg.text }}</p>
          <div class="form-actions">
            <button type="button" class="btn" :disabled="createDsSubmitting" @click="closeCreateDataset">
              取消
            </button>
            <button type="submit" class="btn btn-primary" :disabled="createDsSubmitting">提交</button>
          </div>
        </form>
      </div>
    </div>

    <!-- ============ 创建快照对话框 ============ -->
    <div v-if="showCreateSnapshot" class="modal-backdrop" @click.self="closeCreateSnapshot">
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="create-snap-title">
        <div class="modal-head">
          <h3 id="create-snap-title">创建快照</h3>
          <button class="modal-close" type="button" :disabled="createSnapSubmitting" @click="closeCreateSnapshot">
            ×
          </button>
        </div>
        <form class="modal-body" @submit.prevent="submitCreateSnapshot">
          <div class="field">
            <label for="snap-dataset">数据集</label>
            <select id="snap-dataset" v-model="createSnapForm.dataset" :disabled="createSnapSubmitting">
              <option value="" disabled>请选择数据集…</option>
              <option v-for="d in datasets" :key="d.id" :value="d.name">{{ d.name }}</option>
            </select>
            <p v-if="!datasets.length" class="hint hint-warn">尚未加载到任何数据集，请先创建数据集。</p>
          </div>
          <div class="field">
            <label for="snap-name">快照名</label>
            <input
              id="snap-name"
              v-model="createSnapForm.name"
              type="text"
              placeholder="例如 snap-20260809"
              :disabled="createSnapSubmitting"
            />
          </div>
          <p v-if="createSnapMsg" :class="['form-msg', `is-${createSnapMsg.kind}`]">{{ createSnapMsg.text }}</p>
          <div class="form-actions">
            <button type="button" class="btn" :disabled="createSnapSubmitting" @click="closeCreateSnapshot">
              取消
            </button>
            <button type="submit" class="btn btn-primary" :disabled="createSnapSubmitting">提交</button>
          </div>
        </form>
      </div>
    </div>

    <!-- ============ 池详情对话框 ============ -->
    <div v-if="detailPool" class="modal-backdrop" @click.self="closePoolDetail">
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="pool-detail-title">
        <div class="modal-head">
          <h3 id="pool-detail-title">池详情 · {{ detailPool.name || detailPool.id }}</h3>
          <button class="modal-close" type="button" @click="closePoolDetail">×</button>
        </div>
        <div class="modal-body">
          <div class="detail-row">
            <span class="detail-label">健康状态</span>
            <HealthBadge :health="detailPool.health" />
          </div>
          <div class="detail-row">
            <span class="detail-label">使用率</span>
            <CapacityBar :capacity="detailPool.capacity" :show-text="true" />
          </div>
          <div class="detail-row">
            <span class="detail-label">总容量</span>
            <span class="mono">{{ formatBytes(detailPool.capacity?.total_bytes) }}</span>
          </div>
          <div class="detail-row">
            <span class="detail-label">已用</span>
            <span class="mono">{{ formatBytes(detailPool.capacity?.used_bytes) }}（{{ poolPct(detailPool) }}%）</span>
          </div>
          <div class="detail-block">
            <div class="detail-label">vdev 布局</div>
            <div v-if="(detailPool.vdevs || []).length" class="vdev-list">
              <div v-for="(v, i) in detailPool.vdevs" :key="i" class="vdev-item">
                <div class="vdev-head">
                  <span class="vdev-kind">{{ v.kind }}</span>
                  <HealthBadge :health="v.health" />
                  <span class="vdev-errs" v-if="v.read_errors || v.write_errors || v.cksum_errors">
                    错误 读/{{ v.read_errors }} 写/{{ v.write_errors }} 校验/{{ v.cksum_errors }}
                  </span>
                </div>
                <div class="vdev-disks mono">
                  <span v-for="(dsk, j) in v.disks" :key="j" class="disk-chip">{{ dsk }}</span>
                  <span v-if="!v.disks || !v.disks.length" class="muted">（无磁盘）</span>
                </div>
              </div>
            </div>
            <p v-else class="muted">该池暂无 vdev 信息。</p>
          </div>
          <div class="form-actions">
            <button
              type="button"
              class="btn btn-danger"
              :disabled="deleteSubmitting"
              @click="openDeletePool(detailPool)"
            >
              删除池…
            </button>
            <button type="button" class="btn" @click="closePoolDetail">关闭</button>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.storage-page {
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

.head-actions {
  display: flex;
  align-items: center;
  gap: 12px;
  flex-wrap: wrap;
}

/* —— Tab —— */
.tabs {
  display: flex;
  gap: 4px;
  border-bottom: 1px solid var(--border-soft, #EDEDED);
}

.tab-btn {
  padding: 9px 16px;
  border: 1px solid transparent;
  border-bottom: none;
  border-radius: var(--radius-sm, 8px) var(--radius-sm, 8px) 0 0;
  background: transparent;
  color: var(--text-muted, #6b7280);
  cursor: pointer;
  font-size: 14px;
  font-family: inherit;
  transition: background 0.15s ease, color 0.15s ease;
  position: relative;
  top: 1px;
}

.tab-btn:hover {
  color: var(--text, #2B2B2B);
  background: rgba(0, 0, 0, 0.03);
}

.tab-btn.active {
  color: var(--accent, #E95420);
  border-color: var(--border-soft, #EDEDED);
  background: var(--bg-card, #ffffff);
  font-weight: 600;
}

/* —— 面板 —— */
.panel {
  display: flex;
  flex-direction: column;
  gap: 12px;
}

.panel-head {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 12px;
  flex-wrap: wrap;
}

.panel-head h3 {
  font-size: 16px;
  font-weight: 600;
  color: var(--text, #2B2B2B);
}

.filter-group {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 13px;
  color: var(--text, #2B2B2B);
}

.filter-group select {
  padding: 5px 10px;
  border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px);
  background: var(--bg-card, #ffffff);
  font-family: inherit;
  font-size: 13px;
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

/* —— ZFS 工具缺失的低调信息条（info 样式，区别于 error-box 红幅）—— */
.info-box {
  color: #1d4ed8;
  background: #dbeafe;
  border: 1px solid rgba(29, 78, 216, 0.2);
  padding: 10px 14px;
  border-radius: var(--radius-sm, 8px);
  font-size: 13px;
}

/* —— pill 小徽章 —— */
.pill {
  display: inline-block;
  padding: 2px 10px;
  border-radius: var(--radius-pill, 20px);
  font-size: 12px;
  font-weight: 600;
}
.pill-ok { color: #15803d; background: #dcfce7; }
.pill-muted { color: #6b7280; background: #f3f4f6; }
.pill-healthy { color: #15803d; background: #dcfce7; }
.pill-unhealthy { color: #b91c1c; background: #fee2e2; }
.pill-unknown { color: #475569; background: #e2e8f0; }

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
.btn:hover { background: rgba(0, 0, 0, 0.04); }
.btn:disabled { opacity: 0.5; cursor: not-allowed; }
.btn-small { padding: 4px 10px; font-size: 12.5px; }
.btn-primary {
  background: var(--accent, #E95420);
  color: #ffffff;
  border-color: var(--accent, #E95420);
}
.btn-primary:hover { background: var(--accent-hi, #0077ed); }
.btn-danger {
  color: #b91c1c;
  border-color: rgba(185, 28, 28, 0.35);
  background: #fff5f5;
}
.btn-danger:hover:not(:disabled) { background: #fee2e2; }
.strong-btn { font-weight: 600; }

.link-btn {
  background: transparent;
  border: none;
  color: var(--accent, #E95420);
  cursor: pointer;
  font-size: 13px;
  font-family: inherit;
  padding: 2px 0;
}
.link-btn:hover { text-decoration: underline; }

/* 刷新按钮里的图标旋转 */
.spin { display: inline-block; font-size: 14px; line-height: 1; }
.spin.spinning { animation: spin 0.8s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }

/* —— 模态框 —— */
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
  width: min(560px, 100%);
  max-height: 90vh;
  overflow: auto;
  background: var(--bg-card, #ffffff);
  border-radius: var(--radius, 16px);
  box-shadow: var(--shadow-modal, 0 20px 60px rgba(0, 0, 0, 0.25));
  display: flex;
  flex-direction: column;
}

.modal-wizard {
  width: min(920px, 100%);
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
.modal-close:hover:not(:disabled) { color: var(--text, #2B2B2B); }

.modal-body {
  padding: 18px 20px;
  display: flex;
  flex-direction: column;
  gap: 14px;
}

/* —— 向导 stepper —— */
.stepper {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 14px 20px;
  border-bottom: 1px solid var(--border-soft, #EDEDED);
  background: var(--bg-app, #FAFAFA);
}
.step {
  display: flex;
  align-items: center;
  gap: 8px;
  font-size: 13px;
  color: var(--text-muted, #6b7280);
}
.step:not(:last-child)::after {
  content: '';
  display: inline-block;
  width: 28px;
  height: 1px;
  background: var(--border, #D9D9D9);
  margin: 0 6px;
}
.step-num {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  width: 22px;
  height: 22px;
  border-radius: 50%;
  border: 1px solid var(--border, #D9D9D9);
  font-size: 12px;
  font-weight: 600;
  background: var(--bg-card, #ffffff);
}
.step.active { color: var(--accent, #E95420); font-weight: 600; }
.step.active .step-num {
  border-color: var(--accent, #E95420);
  color: #fff;
  background: var(--accent, #E95420);
}
.step.done { color: #15803d; }
.step.done .step-num {
  border-color: #15803d;
  color: #fff;
  background: #15803d;
}

/* —— 向导左右分栏 —— */
.wizard-split {
  display: grid;
  grid-template-columns: 55fr 45fr;
  gap: 16px;
  min-height: 320px;
}
.split-left,
.split-right {
  display: flex;
  flex-direction: column;
  gap: 10px;
  min-width: 0;
}
.split-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
}
.split-title {
  font-size: 14px;
  font-weight: 600;
  color: var(--text, #2B2B2B);
}

/* 第一步：硬盘卡片网格 */
.disk-grid {
  display: flex;
  flex-direction: column;
  gap: 8px;
  max-height: 340px;
  overflow-y: auto;
  padding-right: 2px;
}
.disk-card {
  display: flex;
  align-items: center;
  gap: 10px;
  padding: 10px 12px;
  border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px);
  background: var(--bg-card, #ffffff);
  cursor: pointer;
  transition: border-color 0.12s ease, background 0.12s ease;
}
.disk-card:hover { border-color: var(--accent, #E95420); }
.disk-card.checked {
  border-color: var(--accent, #E95420);
  background: rgba(233, 84, 32, 0.06);
}
.disk-card input[type='checkbox'] {
  width: 16px;
  height: 16px;
  cursor: pointer;
  flex-shrink: 0;
}
.disk-card-main {
  display: flex;
  flex-direction: column;
  gap: 2px;
  min-width: 0;
  flex: 1;
}
.disk-card-row {
  display: flex;
  align-items: center;
  gap: 8px;
}
.disk-card-sub {
  display: flex;
  gap: 6px;
  font-size: 12.5px;
  color: var(--text-muted, #6b7280);
}
.disk-size { color: var(--text, #2B2B2B); font-weight: 500; }
.disk-model { color: var(--text-muted, #6b7280); }

/* —— 未初始化盘（残留分区/签名）：灰显 + 警告条 —— */
.disk-item {
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.disk-card.needs-init {
  opacity: 0.62;
  cursor: not-allowed;
}
.disk-card.needs-init:hover { border-color: var(--border, #d1d5db); }
.disk-card.needs-init input[type='checkbox'] { cursor: not-allowed; }
.init-flag {
  display: inline-block;
  padding: 1px 8px;
  border-radius: var(--radius-pill, 20px);
  font-size: 11px;
  font-weight: 600;
  color: #b45309;
  background: #fef3c7;
}
.init-warn {
  display: flex;
  flex-direction: column;
  gap: 6px;
  padding: 8px 12px;
  border: 1px solid rgba(180, 83, 9, 0.35);
  border-radius: var(--radius-sm, 8px);
  background: #fffbeb;
}
.init-warn-text {
  font-size: 12.5px;
  line-height: 1.55;
  color: #92400e;
}
.init-warn-actions {
  display: flex;
  gap: 8px;
}
.btn-warn {
  color: #92400e;
  border-color: rgba(180, 83, 9, 0.4);
  background: #fef3c7;
}
.btn-warn:hover:not(:disabled) { background: #fde68a; }

/* —— 池成员/可导入池盘（2026-08-30 已建池识别与导入）—— */
/* 活跃池成员 & 可导入池成员：与需初始化盘同样灰显禁选 */
.disk-card.is-member,
.disk-card.is-importable {
  opacity: 0.62;
  cursor: not-allowed;
}
.disk-card.is-member:hover,
.disk-card.is-importable:hover { border-color: var(--border, #d1d5db); }
.disk-card.is-member input[type='checkbox'],
.disk-card.is-importable input[type='checkbox'] { cursor: not-allowed; }
/* 徽标：池内成员（灰）/ 属于可导入池（蓝） */
.member-flag { color: #475569; background: #e2e8f0; }
.hint-flag { color: #1d4ed8; background: #dbeafe; }
/* 警告条变体：活跃池（灰）/ 可导入池（蓝——数据完好，别 wipefs） */
.member-warn {
  border-color: rgba(71, 85, 105, 0.35);
  background: #f8fafc;
}
.member-warn .init-warn-text { color: #475569; }
.importable-warn {
  border-color: rgba(29, 78, 216, 0.35);
  background: #eff6ff;
}
.importable-warn .init-warn-text { color: #1e40af; }

/* —— 可导入池横幅（存储池 tab 顶部）—— */
.import-banner {
  display: flex;
  flex-direction: column;
  gap: 8px;
  margin-bottom: 14px;
  padding: 12px 16px;
  border: 1px solid rgba(29, 78, 216, 0.3);
  border-left: 4px solid #2563eb;
  border-radius: var(--radius-sm, 8px);
  background: #eff6ff;
}
.import-banner-head {
  font-size: 13.5px;
  font-weight: 600;
  color: #1e40af;
}
.import-banner-row {
  display: flex;
  align-items: center;
  gap: 10px;
}
.import-banner-name { font-size: 13px; color: #1e3a8a; }
.import-banner-msg {
  font-size: 12.5px;
  margin: 0;
}
.import-banner-msg.msg-ok { color: #15803d; }
.import-banner-msg.msg-err { color: #b91c1c; }
.import-banner-msg.msg-info { color: #1e40af; }

/* —— 分区详情面板 —— */
.part-detail {
  padding: 8px 12px;
  border: 1px dashed var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px);
  background: var(--bg-app, #FAFAFA);
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.part-list {
  display: flex;
  flex-direction: column;
  gap: 2px;
  max-height: 140px;
  overflow-y: auto;
}
.part-row {
  display: grid;
  grid-template-columns: 1.2fr 0.7fr 1fr 1fr;
  gap: 8px;
  font-size: 12px;
  padding: 2px 4px;
  color: var(--text, #2B2B2B);
}
.part-label { color: var(--text-muted, #6b7280); }

/* —— 初始化确认对话框（窄版） —— */
.modal-init {
  width: min(480px, 100%);
}

.type-pill {
  display: inline-block;
  padding: 1px 8px;
  border-radius: var(--radius-pill, 20px);
  font-size: 11px;
  font-weight: 600;
}
.type-ssd { color: #1565c0; background: #e3f2fd; }
.type-hdd { color: #6b7280; background: #f3f4f6; }

/* 第一步右侧摘要 */
.summary-card {
  border: 1px solid var(--border-soft, #EDEDED);
  border-radius: var(--radius-sm, 8px);
  padding: 12px 14px;
  background: var(--bg-app, #FAFAFA);
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.summary-row {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  font-size: 13px;
}
.summary-row.big { font-size: 16px; font-weight: 600; }
.summary-label { color: var(--text-muted, #6b7280); }
.summary-val { color: var(--text, #2B2B2B); }

.mode-hints { display: flex; flex-direction: column; gap: 6px; }
.hint-list {
  margin: 0;
  padding-left: 18px;
  display: flex;
  flex-direction: column;
  gap: 4px;
  font-size: 12px;
  color: var(--text-muted, #6b7280);
  line-height: 1.55;
}
.hint-list code {
  background: rgba(0, 0, 0, 0.05);
  padding: 1px 5px;
  border-radius: 3px;
  font-family: var(--mono, monospace);
  font-size: 11.5px;
}

/* 第二步：模式卡片 */
.mode-grid {
  display: flex;
  flex-direction: column;
  gap: 10px;
  max-height: 380px;
  overflow-y: auto;
  padding-right: 2px;
}
.mode-card {
  text-align: left;
  padding: 12px 14px;
  border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px);
  background: var(--bg-card, #ffffff);
  cursor: pointer;
  font-family: inherit;
  transition: border-color 0.12s ease, background 0.12s ease, box-shadow 0.12s ease;
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.mode-card:hover { border-color: var(--accent, #E95420); }
.mode-card.selected {
  border-color: var(--accent, #E95420);
  background: rgba(233, 84, 32, 0.05);
  box-shadow: 0 0 0 1px var(--accent, #E95420) inset;
}
.mode-card-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
}
.mode-name { font-size: 14px; font-weight: 600; color: var(--text, #2B2B2B); }
.mode-check { color: var(--accent, #E95420); font-weight: 700; }
.mode-cap { font-size: 13px; color: var(--text, #2B2B2B); }
.mode-cap strong { color: var(--accent, #E95420); }
.mode-desc { font-size: 12px; color: var(--text-muted, #6b7280); line-height: 1.5; margin: 0; }
.mode-fault { font-size: 12px; color: #15803d; margin: 0; }

/* 文件系统 radio */
.radio-row {
  display: flex;
  gap: 8px;
  flex-wrap: wrap;
}
.radio-item {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 5px 12px;
  border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-pill, 20px);
  font-size: 13px;
  cursor: pointer;
  background: var(--bg-card, #ffffff);
}
.radio-item.on {
  border-color: var(--accent, #E95420);
  color: var(--accent, #E95420);
  background: rgba(233, 84, 32, 0.06);
}
.radio-item.disabled {
  opacity: 0.55;
  cursor: not-allowed;
}
.radio-item em {
  font-style: normal;
  font-size: 11px;
  color: var(--text-muted, #6b7280);
}
.radio-item input[type='radio'] {
  width: 14px;
  height: 14px;
  cursor: pointer;
}

/* 高级折叠 */
.advanced-toggle { margin-top: 2px; }
.advanced-box {
  border: 1px dashed var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px);
  padding: 10px 12px;
  display: flex;
  flex-direction: column;
  gap: 10px;
  background: var(--bg-app, #FAFAFA);
}
.mini-disk-list {
  display: flex;
  flex-direction: column;
  gap: 4px;
  max-height: 120px;
  overflow-y: auto;
}
.mini-disk-row {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 4px 6px;
  border-radius: 4px;
  font-size: 12.5px;
  cursor: pointer;
}
.mini-disk-row:hover { background: rgba(0, 0, 0, 0.04); }
.mini-disk-row.checked { background: rgba(233, 84, 32, 0.08); }
.mini-disk-row input[type='checkbox'] { width: 14px; height: 14px; cursor: pointer; }
.disk-meta { color: var(--text-muted, #6b7280); font-size: 12px; }

/* —— 第三步 确认 —— */
.confirm-wrap { display: flex; flex-direction: column; gap: 12px; }
.confirm-card {
  border: 1px solid var(--border-soft, #EDEDED);
  border-radius: var(--radius-sm, 8px);
  padding: 14px 16px;
  background: var(--bg-app, #FAFAFA);
  display: flex;
  flex-direction: column;
  gap: 12px;
}
.confirm-section { display: flex; flex-direction: column; gap: 6px; }
.confirm-grid {
  display: grid;
  grid-template-columns: 1fr 1fr;
  gap: 8px 16px;
}
.confirm-row {
  display: flex;
  flex-direction: column;
  gap: 2px;
  font-size: 13px;
}
.confirm-label { font-size: 12px; color: var(--text-muted, #6b7280); }
.strong { font-weight: 600; color: var(--accent, #E95420); }
.chip-row { display: flex; flex-wrap: wrap; gap: 6px; }
.disk-chip {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 3px 10px;
  background: var(--bg-card, #ffffff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-pill, 20px);
  font-size: 12.5px;
}
.chip-meta { color: var(--text-muted, #6b7280); font-size: 11.5px; }

.warn-box {
  color: #b91c1c;
  background: #fee2e2;
  border: 1px solid rgba(185, 28, 28, 0.3);
  padding: 10px 14px;
  border-radius: var(--radius-sm, 8px);
  font-size: 13px;
  line-height: 1.6;
}

/* —— 删除池确认：两种模式单选（export 默认 / wipe 红色警示） —— */
.del-modes {
  display: flex;
  flex-direction: column;
  gap: 8px;
}

.del-mode {
  display: flex;
  gap: 10px;
  align-items: flex-start;
  padding: 10px 12px;
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-sm, 8px);
  cursor: pointer;
  transition: border-color 0.15s ease, background 0.15s ease;
}

.del-mode:hover {
  border-color: var(--accent, #E95420);
}

.del-mode.checked {
  border-color: var(--accent, #E95420);
  background: rgba(233, 84, 32, 0.05);
}

.del-mode.is-danger.checked {
  border-color: #b91c1c;
  background: #fee2e2;
}

.del-mode input[type='radio'] {
  margin-top: 3px;
  accent-color: var(--accent, #E95420);
}

.del-mode.is-danger input[type='radio'] {
  accent-color: #b91c1c;
}

.del-mode-body {
  display: flex;
  flex-direction: column;
  gap: 3px;
  font-size: 13px;
  line-height: 1.55;
}

.danger-text {
  color: #b91c1c;
}

/* —— 表单字段 —— */
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

.field input,
.field textarea,
.field select {
  width: 100%;
  padding: 7px 10px;
  border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px);
  font-family: inherit;
  font-size: 14px;
  color: var(--text, #2B2B2B);
  background: var(--bg-card, #ffffff);
}

.field textarea {
  font-family: var(--mono, monospace);
  resize: vertical;
}

.field input:focus,
.field textarea:focus,
.field select:focus {
  outline: none;
  border-color: var(--accent, #E95420);
  box-shadow: 0 0 0 3px rgba(233, 84, 32, 0.15);
}

.hint {
  font-size: 12px;
  color: var(--text-muted, #6b7280);
  line-height: 1.6;
}
.hint-warn { color: #b45309; }

.hint code {
  background: rgba(0, 0, 0, 0.05);
  padding: 1px 5px;
  border-radius: 3px;
  font-family: var(--mono, monospace);
  font-size: 11.5px;
}

.form-msg {
  font-size: 13px;
  padding: 6px 0;
}
.form-msg.is-err { color: #b91c1c; }
.form-msg.is-ok { color: #15803d; }
.form-msg.is-info { color: var(--text-muted, #6b7280); }

.form-actions {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
  margin-top: 4px;
}

/* —— 池详情 —— */
.detail-row {
  display: flex;
  align-items: center;
  gap: 12px;
  font-size: 13.5px;
}
.detail-label {
  min-width: 88px;
  color: var(--text-muted, #6b7280);
  font-size: 12.5px;
}
.detail-block {
  border-top: 1px solid var(--border-soft, #EDEDED);
  padding-top: 12px;
  display: flex;
  flex-direction: column;
  gap: 8px;
}
.vdev-list {
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.vdev-item {
  border: 1px solid var(--border-soft, #EDEDED);
  border-radius: var(--radius-sm, 8px);
  padding: 10px 12px;
  background: var(--bg-app, #FAFAFA);
}
.vdev-head {
  display: flex;
  align-items: center;
  gap: 10px;
  margin-bottom: 6px;
}
.vdev-kind {
  font-family: var(--mono, monospace);
  font-size: 12.5px;
  font-weight: 600;
  color: var(--accent, #E95420);
  text-transform: uppercase;
}
.vdev-errs {
  font-size: 11.5px;
  color: #b91c1c;
}
.vdev-disks {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
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
  .storage-page { padding: 16px; }
  .wizard-split { grid-template-columns: 1fr; }
  .confirm-grid { grid-template-columns: 1fr; }
}
</style>
