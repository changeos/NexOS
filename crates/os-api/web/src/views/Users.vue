<!--
  Users.vue —— 用户管理页面
  数据来源：os-api `/api/v1/users` 路由（UserRouteHandler）。
  Vue3 移植自 crates/os-api/static/js/users.js。
-->
<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { api, ApiError } from '@/api';
import type { UserInfo } from '@/types';
import { roleBadgeClass } from '@/composables/useFormat';
import { useToast } from '@/composables/useToast';

const toast = useToast();

// —— 列表状态 ——
const users = ref<UserInfo[]>([]);
const loading = ref(false);
const errorMsg = ref('');
const includeDisabled = ref(false);

// —— 创建对话框状态 ——
const showCreate = ref(false);
const formName = ref('');
const formRoles = ref('operator');
const formIsGuest = ref(false);
const formEnabled = ref(true);
const creating = ref(false);
const createMsg = ref('');

const parsedRoles = computed<string[]>(() => {
  return formRoles.value
    .split(',')
    .map((r) => r.trim())
    .filter(Boolean);
});

function openCreate(): void {
  formName.value = '';
  formRoles.value = 'operator';
  formIsGuest.value = false;
  formEnabled.value = true;
  createMsg.value = '';
  showCreate.value = true;
}

function closeCreate(): void {
  showCreate.value = false;
}

// —— API 调用 ——
async function load(): Promise<void> {
  loading.value = true;
  errorMsg.value = '';
  try {
    users.value = ((await api.users(includeDisabled.value)) as unknown as UserInfo[]) ?? [];
  } catch (e) {
    errorMsg.value = errMsg(e);
  } finally {
    loading.value = false;
  }
}

async function onCreate(): Promise<void> {
  const name = formName.value.trim();
  if (!name) {
    createMsg.value = '用户名不能为空';
    return;
  }
  const body: UserInfo = {
    id: name,
    name,
    roles: parsedRoles.value,
    enabled: formEnabled.value,
    is_guest: formIsGuest.value,
  };
  createMsg.value = '创建中…';
  creating.value = true;
  try {
    await api.createUser(body);
    showCreate.value = false;
    toast.success('用户已创建');
    await load();
  } catch (e) {
    createMsg.value = `创建失败: ${errMsg(e)}`;
  } finally {
    creating.value = false;
  }
}

async function onDelete(u: UserInfo): Promise<void> {
  if (!window.confirm(`确认删除用户 ${u.id}？`)) return;
  try {
    await api.deleteUser(u.id);
    toast.success('用户已删除');
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
  <div class="users-page">
    <div class="page-head">
      <div>
        <h2>用户</h2>
        <div class="page-sub">系统用户与角色管理</div>
      </div>
      <div class="row-gap-sm">
        <label class="form-check muted small" style="flex-direction: row">
          <input v-model="includeDisabled" type="checkbox" @change="load" />
          包含已禁用
        </label>
        <button class="btn btn-primary" @click="openCreate">＋ 创建用户</button>
      </div>
    </div>

    <div v-if="loading" class="loading">加载中...</div>
    <div v-else-if="errorMsg" class="error">加载失败: {{ errorMsg }}</div>

    <div v-else class="table-wrap">
      <table class="data-table">
        <thead>
          <tr>
            <th>用户名</th>
            <th>角色</th>
            <th>启用</th>
            <th>访客</th>
            <th class="col-actions">操作</th>
          </tr>
        </thead>
        <tbody>
          <tr v-if="!users.length">
            <td colspan="5" class="muted center">暂无用户，点击“创建用户”新增。</td>
          </tr>
          <tr v-for="u in users" :key="u.id">
            <td>
              {{ u.name || u.id }}
              <div class="muted mono small">{{ u.id }}</div>
            </td>
            <td>
              <span v-if="u.roles && u.roles.length" class="row-gap-sm">
                <span
                  v-for="r in u.roles"
                  :key="r"
                  class="badge"
                  :class="roleBadgeClass(r)"
                  >{{ r }}</span
                >
              </span>
              <span v-else class="muted">—</span>
            </td>
            <td>
              <span v-if="u.enabled" class="badge badge-ok">启用</span>
              <span v-else class="badge badge-muted">禁用</span>
            </td>
            <td>
              <span v-if="u.is_guest" class="badge badge-warn">访客</span>
              <span v-else class="muted">—</span>
            </td>
            <td class="col-actions">
              <button
                class="btn btn-small btn-danger"
                @click="onDelete(u)"
              >
                删除
              </button>
            </td>
          </tr>
        </tbody>
      </table>
    </div>

    <!-- 创建用户对话框 -->
    <div v-if="showCreate" class="modal-backdrop" @click.self="closeCreate">
      <div class="modal">
        <div class="modal-head">
          <h3>创建用户</h3>
          <button class="modal-close" @click="closeCreate">×</button>
        </div>
        <div class="modal-body">
          <form class="form" @submit.prevent="onCreate">
            <label>
              用户名
              <input
                v-model="formName"
                type="text"
                placeholder="例如 alice"
                required
              />
            </label>
            <label>
              角色（逗号分隔，如 admin, operator）
              <input
                v-model="formRoles"
                type="text"
                placeholder="operator"
              />
            </label>
            <label class="form-check">
              <input v-model="formIsGuest" type="checkbox" />
              访客身份
            </label>
            <label class="form-check">
              <input v-model="formEnabled" type="checkbox" />
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
  </div>
</template>

<style scoped>
.users-page {
  padding: 20px 24px;
  display: flex;
  flex-direction: column;
  gap: 16px;
}
</style>
