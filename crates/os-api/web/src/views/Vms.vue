<script setup lang="ts">
// =============================================================================
// Vms.vue —— 虚拟机管理
//
// 功能（对齐 static/js/vms.js）：
//   1. VM 列表表格（名称 + id / 状态徽章 / CPU / 内存 / 节点）
//   2. 操作按钮：启动 / 停止 / 删除（按 state 动态禁用；删除二次确认）
//   3. 创建 VM 对话框（名称 + CPU 数 + 内存 MiB + 磁盘 zvol id）
//
// API：
//   GET    /api/v1/vms
//   POST   /api/v1/vms              body: VmSpec
//   POST   /api/v1/vms/:id/start
//   POST   /api/v1/vms/:id/stop
//   DELETE /api/v1/vms/:id
// =============================================================================
import { computed, onMounted, ref, watch } from 'vue';
import { useRouter } from 'vue-router';
import DataTable from '@/components/DataTable.vue';
import type { Column } from '@/components/data-table';
import StatusBadge from '@/components/StatusBadge.vue';
import { endpoints } from '@/api/client';
import { formatMemoryMB } from '@/utils/format';
import type { CreateVmRequest, Pool, Vm, VmState } from '@/api/types';

const router = useRouter();

// —— 列表状态 ——
const vms = ref<Vm[]>([]);
const loading = ref(false);
const errorMsg = ref('');
const toastMsg = ref<{ kind: 'success' | 'error' | 'info'; text: string } | null>(null);
let toastTimer: ReturnType<typeof setTimeout> | null = null;

function showToast(kind: 'success' | 'error' | 'info', text: string): void {
  toastMsg.value = { kind, text };
  if (toastTimer) clearTimeout(toastTimer);
  toastTimer = setTimeout(() => {
    toastMsg.value = null;
  }, 3500);
}

async function loadVms(): Promise<void> {
  loading.value = true;
  errorMsg.value = '';
  try {
    vms.value = await endpoints.vms();
  } catch (e) {
    errorMsg.value = e instanceof Error ? e.message : String(e);
    vms.value = [];
  } finally {
    loading.value = false;
  }
}

// —— 存储池列表（创建 VM 前检查；磁盘选择下拉源）——
const pools = ref<Pool[]>([]);
const poolsLoading = ref(false);

async function loadPools(): Promise<void> {
  poolsLoading.value = true;
  try {
    const result = await endpoints.pools();
    // ZFS 不可用的节点返回 {pools:[], zfs_available:false} 降级空态——按无池处理
    const degraded = result as { pools?: unknown };
    pools.value = Array.isArray(result)
      ? result
      : Array.isArray(degraded.pools)
        ? (degraded.pools as Pool[])
        : [];
  } catch (e) {
    // 加载失败按"无池"处理，由按钮流程二次提示错误细节
    pools.value = [];
    console.warn('loadPools 失败：', e instanceof Error ? e.message : e);
  } finally {
    poolsLoading.value = false;
  }
}

/** 池名列表（用于磁盘下拉）。 */
const poolNames = computed(() => pools.value.map((p) => p.name).filter(Boolean));
/** 首个池名（用于默认磁盘卷 id）。 */
const firstPoolName = computed(() => poolNames.value[0] ?? '');

/** 根据名称前缀拼默认磁盘卷 id：<pool>/vm/<dataset>。 */
function defaultDiskVolId(pool: string, dataset: string): string {
  const ds = dataset.trim() || 'disk1';
  const poolName = pool || firstPoolName.value;
  return poolName ? `${poolName}/vm/${ds}` : '';
}

// —— 无池警告对话框 ——
const showNoPoolWarn = ref(false);

function openNoPoolWarn(): void {
  showNoPoolWarn.value = true;
}

function closeNoPoolWarn(): void {
  showNoPoolWarn.value = false;
}

function gotoCreatePool(): void {
  showNoPoolWarn.value = false;
  void router.push('/storage');
}

// —— 生命周期操作（启动/停止/删除）——
async function startVm(vm: Vm): Promise<void> {
  try {
    await endpoints.vmStart(vm.id);
    await loadVms();
    showToast('success', `虚拟机 ${vm.name || vm.id} 已启动`);
  } catch (e) {
    showToast('error', '启动失败：' + (e instanceof Error ? e.message : String(e)));
  }
}

async function stopVm(vm: Vm): Promise<void> {
  try {
    await endpoints.vmStop(vm.id);
    await loadVms();
    showToast('success', `虚拟机 ${vm.name || vm.id} 已停止`);
  } catch (e) {
    showToast('error', '停止失败：' + (e instanceof Error ? e.message : String(e)));
  }
}

async function deleteVm(vm: Vm): Promise<void> {
  if (!confirm(`确认删除虚拟机 ${vm.name || vm.id}？此操作不可撤销。`)) return;
  try {
    await endpoints.vmDelete(vm.id);
    await loadVms();
    showToast('success', `虚拟机 ${vm.name || vm.id} 已删除`);
  } catch (e) {
    showToast('error', '删除失败：' + (e instanceof Error ? e.message : String(e)));
  }
}

// —— 创建 VM 对话框 ——
const showCreateVm = ref(false);
const createForm = ref({
  name: '',
  cpus: 2,
  memory: 1024,
  diskPool: '',
  diskDataset: '',
});
const createSubmitting = ref(false);
const createMsg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);

/** 点击「创建 VM」按钮：先检查存储池，有池才打开创建表单。 */
async function openCreateVm(): Promise<void> {
  // 先（重新）拉一次池列表，确保检测结果最新
  await loadPools();
  if (pools.value.length === 0) {
    openNoPoolWarn();
    return;
  }
  // 默认填首个池；如含 osprobepersist 则优先选它
  const preferred =
    poolNames.value.find((n) => n === 'osprobepersist') || firstPoolName.value;
  createForm.value = {
    name: '',
    cpus: 2,
    memory: 1024,
    diskPool: preferred,
    diskDataset: '',
  };
  createMsg.value = null;
  showCreateVm.value = true;
}

function closeCreateVm(): void {
  if (createSubmitting.value) return;
  showCreateVm.value = false;
}

/** 表单内磁盘卷 id 预览（= 池名/vm/数据集）。 */
const diskVolIdPreview = computed(() =>
  defaultDiskVolId(createForm.value.diskPool, createForm.value.diskDataset || 'disk1'),
);

async function submitCreateVm(): Promise<void> {
  const { name, cpus, memory, diskPool, diskDataset } = createForm.value;
  if (!cpus || cpus < 1) {
    createMsg.value = { kind: 'err', text: 'CPU 数必须 ≥ 1' };
    return;
  }
  if (!memory || memory < 1) {
    createMsg.value = { kind: 'err', text: '内存必须 ≥ 1 MiB' };
    return;
  }
  if (!diskPool) {
    createMsg.value = { kind: 'err', text: '请选择存储池' };
    return;
  }
  const disk = defaultDiskVolId(diskPool, diskDataset);
  if (!disk) {
    createMsg.value = { kind: 'err', text: '磁盘卷 ID 不能为空' };
    return;
  }

  // 构造 VmSpec（与 vms.js 一致：对称拓扑 + virtio 网卡 + bios 固件）
  const body: CreateVmRequest = {
    cpus: { vcpus: cpus, sockets: 1, cores: cpus, threads: 1 },
    memory_mb: memory,
    disk_vol_id: disk.trim(),
    nics: [{ bridge: 'br0', model: 'virtio' }],
    firmware: 'bios',
  };

  createSubmitting.value = true;
  createMsg.value = { kind: 'info', text: '创建中…' };
  try {
    // name 是 Vm 字段而非 VmSpec 字段（后端按 id 生成 name）；这里把它附到请求里的做法
    // 因后端 VmSpec 不含 name，name 仅作为提示传给后端可选处理。为类型安全起见，
    // 当 name 非空时用 spread 扩展（后端忽略未知字段）。
    const req = name.trim() ? { ...body, name: name.trim() } : body;
    await endpoints.vmCreate(req as CreateVmRequest);
    showCreateVm.value = false;
    await loadVms();
    showToast('success', '虚拟机已创建');
  } catch (e) {
    createMsg.value = {
      kind: 'err',
      text: '创建失败：' + (e instanceof Error ? e.message : String(e)),
    };
  } finally {
    createSubmitting.value = false;
  }
}

// —— 列定义 ——
const columns: Column<Vm>[] = [
  { key: 'name', title: 'VM 名', accessor: (v) => v.name || v.id },
  { key: 'state', title: '状态', width: '120px' },
  { key: 'vcpus', title: 'CPU', width: '90px', align: 'right', sortable: true,
    accessor: (v) => v.spec?.cpus?.vcpus ?? 0 },
  { key: 'memory', title: '内存', width: '110px', align: 'right', sortable: true,
    accessor: (v) => v.spec?.memory_mb ?? 0 },
  { key: 'node_id', title: '节点', width: '120px',
    accessor: (v) => v.node_id ?? '' },
  { key: 'actions', title: '操作', width: '210px', align: 'right' },
];

function isRunning(state: VmState): boolean {
  return String(state || '').toLowerCase() === 'running';
}

/** 空态引导：无 VM 且无池。 */
const showEmptyGuide = computed(
  () => !loading.value && vms.value.length === 0 && pools.value.length === 0,
);

/** 名称变化时若留空，磁盘预览保持默认 disk1；watch 仅用于触发 preview 重算。 */
watch(() => createForm.value.diskDataset, () => {
  /* diskVolIdPreview 已是 computed，watch 仅为可观测点；无副作用 */
});

onMounted(() => {
  void loadVms();
  void loadPools();
});
</script>

<template>
  <div class="vms-page">
    <div class="page-head">
      <h2 class="page-title">虚拟机</h2>
      <button class="btn btn-primary" @click="openCreateVm">＋ 创建 VM</button>
    </div>

    <div v-if="errorMsg" class="error-box">加载失败：{{ errorMsg }}</div>

    <!-- 空态引导：无 VM 且无池 —— 先去创建存储池 -->
    <div v-if="showEmptyGuide" class="card empty-guide">
      <div class="empty-guide-icon">📋</div>
      <h3 class="empty-guide-title">还没有虚拟机</h3>
      <p class="empty-guide-text">需要先创建存储池才能创建虚拟机。</p>
      <div class="empty-guide-actions">
        <button class="btn btn-primary" @click="gotoCreatePool">创建存储池 →</button>
      </div>
    </div>

    <div v-if="!showEmptyGuide" class="card card-table">
      <DataTable :columns="columns" :rows="vms" :loading="loading" empty-text="暂无虚拟机，点击「创建 VM」新增。">
        <template #cell-name="{ row }">
          <div class="vm-name">
            <strong>{{ row.name || row.id }}</strong>
            <div class="muted mono small">{{ row.id }}</div>
          </div>
        </template>
        <template #cell-state="{ row }">
          <StatusBadge :state="row.state" />
        </template>
        <template #cell-vcpus="{ row }">{{ row.spec?.cpus?.vcpus || '—' }} vCPU</template>
        <template #cell-memory="{ row }">{{ formatMemoryMB(row.spec?.memory_mb) }}</template>
        <template #cell-node_id="{ value }">
          <span :class="value ? '' : 'muted'">{{ value || '未调度' }}</span>
        </template>
        <template #cell-actions="{ row }">
          <div class="action-group">
            <button
              class="btn btn-small btn-ok"
              :disabled="isRunning(row.state)"
              @click.stop="startVm(row)"
            >
              启动
            </button>
            <button
              class="btn btn-small btn-warn"
              :disabled="!isRunning(row.state)"
              @click.stop="stopVm(row)"
            >
              停止
            </button>
            <button class="btn btn-small btn-danger" @click.stop="deleteVm(row)">删除</button>
          </div>
        </template>
      </DataTable>
    </div>

    <!-- 创建 VM 对话框 -->
    <div v-if="showCreateVm" class="modal-backdrop" @click.self="closeCreateVm">
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="create-vm-title">
        <div class="modal-head">
          <h3 id="create-vm-title">创建虚拟机</h3>
          <button class="modal-close" type="button" :disabled="createSubmitting" @click="closeCreateVm">
            ×
          </button>
        </div>
        <form class="modal-body" @submit.prevent="submitCreateVm">
          <div class="field">
            <label for="vm-name">名称 <span class="muted small">（可选；服务端按 id 生成）</span></label>
            <input id="vm-name" v-model="createForm.name" type="text" placeholder="例如 my-vm" :disabled="createSubmitting" />
          </div>
          <div class="field">
            <label for="vm-cpus">CPU 数（vCPU）</label>
            <input
              id="vm-cpus"
              v-model.number="createForm.cpus"
              type="number"
              min="1"
              max="128"
              required
              :disabled="createSubmitting"
            />
          </div>
          <div class="field">
            <label for="vm-memory">内存（MiB）</label>
            <input
              id="vm-memory"
              v-model.number="createForm.memory"
              type="number"
              min="1"
              required
              :disabled="createSubmitting"
            />
          </div>
          <div class="field">
            <label for="vm-disk-pool">
              存储池
              <span class="muted small">（磁盘镜像所在池）</span>
            </label>
            <select
              id="vm-disk-pool"
              v-model="createForm.diskPool"
              required
              :disabled="createSubmitting"
            >
              <option v-for="name in poolNames" :key="name" :value="name">{{ name }}</option>
            </select>
          </div>
          <div class="field">
            <label for="vm-disk-dataset">
              数据集名 <span class="muted small">（可选；默认 disk1）</span>
            </label>
            <input
              id="vm-disk-dataset"
              v-model="createForm.diskDataset"
              type="text"
              placeholder="例如 my-vm 或 disk1"
              :disabled="createSubmitting"
            />
            <span class="muted small mono disk-preview">卷 ID 预览：{{ diskVolIdPreview || '—' }}</span>
          </div>
          <p v-if="createMsg" :class="['form-msg', `is-${createMsg.kind}`]">{{ createMsg.text }}</p>
          <div class="form-actions">
            <button type="button" class="btn" :disabled="createSubmitting" @click="closeCreateVm">取消</button>
            <button type="submit" class="btn btn-primary" :disabled="createSubmitting">创建</button>
          </div>
        </form>
      </div>
    </div>

    <!-- 无存储池警告对话框 -->
    <div v-if="showNoPoolWarn" class="modal-backdrop" @click.self="closeNoPoolWarn">
      <div class="modal modal-warn" role="alertdialog" aria-modal="true" aria-labelledby="no-pool-title">
        <div class="modal-head">
          <h3 id="no-pool-title">⚠️ 尚未创建存储池</h3>
          <button class="modal-close" type="button" @click="closeNoPoolWarn">×</button>
        </div>
        <div class="modal-body">
          <p class="warn-text">
            虚拟机需要存储池来存放磁盘镜像。当前系统没有可用的存储池。
          </p>
          <div class="form-actions">
            <button type="button" class="btn" @click="closeNoPoolWarn">取消</button>
            <button type="button" class="btn btn-primary" @click="gotoCreatePool">
              前往创建存储池 →
            </button>
          </div>
        </div>
      </div>
    </div>

    <!-- Toast -->
    <Transition name="toast">
      <div v-if="toastMsg" :class="['toast', `toast-${toastMsg.kind}`]">{{ toastMsg.text }}</div>
    </Transition>
  </div>
</template>

<style scoped>
.vms-page {
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
  flex-wrap: wrap;
}

.page-title {
  font-size: 22px;
  font-weight: 700;
  color: var(--text, #2B2B2B);
  letter-spacing: -0.02em;
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

.vm-name {
  display: flex;
  flex-direction: column;
  gap: 2px;
}

.muted {
  color: var(--text-muted, #5E5C5F);
}
.small {
  font-size: 12px;
}
.mono {
  font-family: var(--mono, monospace);
}

.action-group {
  display: inline-flex;
  gap: 6px;
  justify-content: flex-end;
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
.btn:hover:not(:disabled) {
  background: rgba(0, 0, 0, 0.04);
}
.btn:disabled {
  opacity: 0.45;
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
.btn-ok {
  background: var(--ok, #0E8420);
  color: #ffffff;
  border-color: var(--ok, #0E8420);
}
.btn-ok:hover:not(:disabled) {
  filter: brightness(0.94);
}
.btn-warn {
  background: var(--warn, #F99B11);
  color: #ffffff;
  border-color: var(--warn, #F99B11);
}
.btn-warn:hover:not(:disabled) {
  filter: brightness(0.94);
}
.btn-danger {
  background: var(--err, #C7162B);
  color: #ffffff;
  border-color: var(--err, #C7162B);
}
.btn-danger:hover:not(:disabled) {
  filter: brightness(0.94);
}

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
  width: min(520px, 100%);
  max-height: 90vh;
  overflow: auto;
  background: var(--bg-card, #ffffff);
  border-radius: var(--radius, 16px);
  box-shadow: var(--shadow-modal, 0 20px 60px rgba(0, 0, 0, 0.25));
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
.field select:focus {
  outline: none;
  border-color: var(--accent, #E95420);
  box-shadow: 0 0 0 3px rgba(233, 84, 32, 0.15);
}

.disk-preview {
  margin-top: 2px;
  display: inline-block;
}

/* —— 空态引导卡片 —— */
.empty-guide {
  padding: 36px 24px;
  text-align: center;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 8px;
}
.empty-guide-icon {
  font-size: 40px;
  line-height: 1;
}
.empty-guide-title {
  font-size: 18px;
  font-weight: 600;
  color: var(--text, #2B2B2B);
  margin: 4px 0 0;
}
.empty-guide-text {
  font-size: 13.5px;
  color: var(--text-muted, #5E5C5F);
  margin: 0 0 8px;
}
.empty-guide-actions {
  display: flex;
  gap: 8px;
}

/* —— 无池警告 —— */
.modal-warn .warn-text {
  font-size: 14px;
  line-height: 1.6;
  color: var(--text, #2B2B2B);
  margin: 0;
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

/* —— Toast —— */
.toast {
  position: fixed;
  bottom: 24px;
  left: 50%;
  transform: translateX(-50%);
  padding: 10px 18px;
  border-radius: var(--radius-pill, 20px);
  font-size: 13.5px;
  color: #ffffff;
  box-shadow: var(--shadow-lg, 0 8px 28px rgba(0, 0, 0, 0.16));
  z-index: 200;
  max-width: 90vw;
}
.toast-success {
  background: var(--ok, #0E8420);
}
.toast-error {
  background: var(--err, #C7162B);
}
.toast-info {
  background: var(--accent, #E95420);
}

.toast-enter-active,
.toast-leave-active {
  transition: opacity 0.25s ease, transform 0.25s ease;
}
.toast-enter-from,
.toast-leave-to {
  opacity: 0;
  transform: translate(-50%, 12px);
}

@media (max-width: 720px) {
  .vms-page {
    padding: 16px;
  }
  .action-group {
    flex-wrap: wrap;
  }
}
</style>
