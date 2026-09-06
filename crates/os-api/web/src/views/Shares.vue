<!--
  Shares.vue —— 文件共享管理页面（SMB / NFS / WebDAV）
  数据来源：os-api `/shares` 路由（ShareRouteHandler）。
  Vue3 移植自 crates/os-api/static/js/shares.js。
-->
<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { useRouter } from 'vue-router';
import { api, ApiError } from '@/api';
import type { ShareInfo } from '@/types';
import { protocolBadgeClass } from '@/composables/useFormat';
import { useToast } from '@/composables/useToast';

/** api.pools() 返回的元素类型（与 @/api/index.ts 的 Pool 一致）。 */
type PoolLike = Exclude<Awaited<ReturnType<typeof api.pools>>, unknown[]>;

const toast = useToast();
const router = useRouter();

// —— 列表状态 ——
const shares = ref<ShareInfo[]>([]);
const pools = ref<PoolLike[]>([]);
const loading = ref(false);
const errorMsg = ref('');

// —— 无存储池警告对话框 ——
const showNoPool = ref(false);

// —— 创建对话框状态 ——
const showCreate = ref(false);
const form = ref<ShareInfo>(emptyForm());
const selectedPool = ref('');
const subDir = ref('');
const creating = ref(false);
const createMsg = ref('');

/** 自动拼接的最终路径（池名/子目录名）。 */
const composedPath = computed(() => {
  const p = selectedPool.value.trim();
  const s = subDir.value.trim();
  if (!p) return '';
  return s ? `${p}/${s}` : p;
});

function emptyForm(): ShareInfo {
  return {
    id: '',
    name: '',
    protocol: 'smb',
    path: '',
    read_only: false,
    enabled: true,
  };
}

/** 加载存储池列表（不抛错给页面，失败时按“无池”处理）。 */
async function loadPools(): Promise<PoolLike[]> {
  try {
    const raw = await api.pools();
    const list = Array.isArray(raw) ? raw : raw ? [raw] : [];
    pools.value = list;
    return list;
  } catch {
    pools.value = [];
    return [];
  }
}

async function openCreate(): Promise<void> {
  // 创建共享前先检查是否有存储池（与 VM 页一致）
  const list = await loadPools();
  if (!list.length) {
    showNoPool.value = true;
    return;
  }
  form.value = emptyForm();
  selectedPool.value = list[0]?.name ?? '';
  subDir.value = '';
  createMsg.value = '';
  showCreate.value = true;
}

function closeCreate(): void {
  showCreate.value = false;
}

function goToStorage(): void {
  showNoPool.value = false;
  router.push('/storage');
}

// —— API 调用 ——
async function load(): Promise<void> {
  loading.value = true;
  errorMsg.value = '';
  try {
    const [shareList] = await Promise.all([
      api.shares(),
      loadPools(),
    ]);
    shares.value = ((shareList as unknown as ShareInfo[]) ?? []) as ShareInfo[];
  } catch (e) {
    errorMsg.value = errMsg(e);
  } finally {
    loading.value = false;
  }
}

async function onCreate(): Promise<void> {
  const path = composedPath.value;
  const body = {
    // id 由前端用 name 兜底（与 static/js/shares.js 一致），真实后端可改
    id: form.value.name.trim() || form.value.name,
    name: form.value.name.trim(),
    protocol: form.value.protocol,
    path,
    read_only: form.value.read_only,
    enabled: form.value.enabled,
  };
  if (!body.name) {
    createMsg.value = '共享名不能为空';
    return;
  }
  if (!body.path) {
    createMsg.value = '请选择存储池并填写子目录名';
    return;
  }
  createMsg.value = '创建中…';
  creating.value = true;
  try {
    await api.createShare(body);
    showCreate.value = false;
    toast.success('共享已创建');
    await load();
  } catch (e) {
    createMsg.value = `创建失败: ${errMsg(e)}`;
  } finally {
    creating.value = false;
  }
}

async function onDelete(s: ShareInfo): Promise<void> {
  if (!window.confirm(`确认删除共享 ${s.id}？`)) return;
  try {
    await api.deleteShare(s.id);
    toast.success('共享已删除');
    await load();
  } catch (e) {
    toast.error(`删除失败: ${errMsg(e)}`);
  }
}

function errMsg(e: unknown): string {
  return e instanceof ApiError || e instanceof Error ? e.message : String(e);
}

onMounted(load);
</script>

<template>
  <div class="shares-page">
    <div class="page-head">
      <div>
        <h2>文件共享</h2>
        <div class="page-sub">SMB / NFS / WebDAV 共享目录管理</div>
      </div>
      <button class="btn btn-primary" @click="openCreate">＋ 创建共享</button>
    </div>

    <div v-if="loading" class="loading">加载中...</div>
    <div v-else-if="errorMsg" class="error">加载失败: {{ errorMsg }}</div>

    <div v-else class="table-wrap">
      <table class="data-table">
        <thead>
          <tr>
            <th>共享名</th>
            <th>协议</th>
            <th>路径</th>
            <th>只读</th>
            <th>启用</th>
            <th class="col-actions">操作</th>
          </tr>
        </thead>
        <tbody>
          <tr v-if="!shares.length">
            <td colspan="6" class="center">
              <div v-if="!pools.length" class="empty-guide">
                <div class="empty-icon">📁</div>
                <div>还没有文件共享。需要先创建存储池才能设置共享。</div>
                <button class="btn btn-primary" @click="goToStorage">创建存储池→</button>
              </div>
              <span v-else class="muted">暂无共享，点击“创建共享”新增。</span>
            </td>
          </tr>
          <tr v-for="s in shares" :key="s.id">
            <td>
              {{ s.name || s.id }}
              <div class="muted mono small">{{ s.id }}</div>
            </td>
            <td>
              <span class="badge" :class="protocolBadgeClass(s.protocol)">
                {{ (s.protocol || '—').toLowerCase() }}
              </span>
            </td>
            <td class="mono">{{ s.path }}</td>
            <td>{{ s.read_only ? '✓' : '—' }}</td>
            <td>
              <span v-if="s.enabled" class="badge badge-ok">启用</span>
              <span v-else class="badge badge-muted">禁用</span>
            </td>
            <td class="col-actions">
              <button
                class="btn btn-small btn-danger"
                @click="onDelete(s)"
              >
                删除
              </button>
            </td>
          </tr>
        </tbody>
      </table>
    </div>

    <!-- 创建共享对话框 -->
    <div v-if="showCreate" class="modal-backdrop" @click.self="closeCreate">
      <div class="modal">
        <div class="modal-head">
          <h3>创建共享</h3>
          <button class="modal-close" @click="closeCreate">×</button>
        </div>
        <div class="modal-body">
          <form class="form" @submit.prevent="onCreate">
            <label>
              共享名
              <input
                v-model="form.name"
                type="text"
                placeholder="例如 media"
                required
              />
            </label>
            <label>
              协议
              <select v-model="form.protocol">
                <option value="smb">SMB</option>
                <option value="nfs">NFS</option>
                <option value="webdav">WebDAV</option>
              </select>
            </label>
            <label>
              存储池
              <select v-model="selectedPool" required>
                <option value="" disabled>请选择存储池</option>
                <option v-for="p in pools" :key="p.name" :value="p.name">
                  {{ p.name }}
                </option>
              </select>
            </label>
            <label>
              子目录名
              <input
                v-model="subDir"
                type="text"
                placeholder="例如 media"
                required
              />
            </label>
            <div class="form-msg muted small">
              最终路径：<span class="mono">{{ composedPath || '（请选择池并填写子目录名）' }}</span>
            </div>
            <label class="form-check">
              <input v-model="form.read_only" type="checkbox" />
              只读
            </label>
            <label class="form-check">
              <input v-model="form.enabled" type="checkbox" />
              启用
            </label>
            <div class="form-actions">
              <button
                type="button"
                class="btn"
                :disabled="creating"
                @click="closeCreate"
              >
                取消
              </button>
              <button type="submit" class="btn btn-primary" :disabled="creating">
                创建
              </button>
            </div>
            <div class="form-msg muted small">{{ createMsg }}</div>
          </form>
        </div>
      </div>
    </div>

    <!-- 无存储池警告对话框 -->
    <div v-if="showNoPool" class="modal-backdrop" @click.self="showNoPool = false">
      <div class="modal">
        <div class="modal-head">
          <h3>需要先创建存储池</h3>
          <button class="modal-close" @click="showNoPool = false">×</button>
        </div>
        <div class="modal-body">
          <p>尚未创建存储池，文件共享需要存储池来存放数据。是否现在创建？</p>
          <div class="form-actions">
            <button type="button" class="btn" @click="showNoPool = false">取消</button>
            <button type="button" class="btn btn-primary" @click="goToStorage">前往创建存储池→</button>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.shares-page {
  padding: 20px 24px;
  display: flex;
  flex-direction: column;
  gap: 16px;
}
.empty-guide {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 10px;
  padding: 20px 12px;
}
.empty-guide .empty-icon {
  font-size: 36px;
  line-height: 1;
}
</style>
