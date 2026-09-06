<!--
  Settings.vue —— 系统设置页面
  - OS 名称 / 语言（中/繁中/英/日）/ 时区 / 管理员信息（前端本地视图）
  - CPU 虚拟化检测详情：调 GET /api/v1/system/virt-check（VirtCheckResult 全字段）
-->
<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue';
import { api, ApiError } from '@/api';
import { getApiToken, setApiToken, endpoints, get, post } from '@/api/client';
import i18n, { setLocale } from '@/i18n';
import type { SystemSettings, VirtCheckResult } from '@/types';
import { cpuVendorLabel, nestedVirtLabel } from '@/composables/useFormat';
import { useToast } from '@/composables/useToast';
import { useWallpaper } from '@/composables/useWallpaper';

/** 系统版本（/update/status 只读展示；设置页不再藏版本号）。 */
const sysVersion = ref('…');
const versionNote = ref('正在读取…');
onMounted(async () => {
  try {
    const st = await endpoints.updateStatus();
    sysVersion.value = `NexOS v${st.current_version}`;
    versionNote.value = `更新通道：${st.channel} · 槽位 ${st.active_slot?.toUpperCase?.() ?? st.active_slot} 活跃`;
  } catch {
    sysVersion.value = '未知';
    versionNote.value = '版本服务未响应（/update/status）';
  }
});


const toast = useToast();

// —— 壁纸（CSS 渐变，localStorage 持久化，实时生效）——
const { current: currentWallpaper, isLight: wallpaperIsLight, setWallpaper, wallpapers } = useWallpaper();

// —— 系统设置（前端本地，写入 localStorage 持久化）——
const STORAGE_KEY = 'os-web-settings';
const settings = ref<SystemSettings>(defaultSettings());
const saving = ref(false);

const LANGUAGES: Array<{ value: string; label: string }> = [
  { value: 'zh-CN', label: '简体中文' },
  { value: 'zh-TW', label: '繁體中文' },
  { value: 'en-US', label: 'English' },
  { value: 'ja-JP', label: '日本語' },
];

const COMMON_TIMEZONES: string[] = [
  'Asia/Shanghai',
  'Asia/Tokyo',
  'UTC',
  'America/Los_Angeles',
  'America/New_York',
  'Europe/London',
];

function defaultSettings(): SystemSettings {
  return {
    osName: 'NexOS',
    language: 'zh-CN',
    timezone: 'Asia/Shanghai',
    admin: { name: 'admin' },
  };
}

function loadSettings(): void {
  try {
    const raw = window.localStorage.getItem(STORAGE_KEY);
    if (raw) {
      const parsed = JSON.parse(raw) as Partial<SystemSettings>;
      settings.value = { ...defaultSettings(), ...parsed };
    }
  } catch {
    /* 忽略损坏的本地配置 */
  }
  // 语言以 i18n 当前值为准（启动时从 os.locale 恢复，顶栏切换实时同步），
  // 避免本地下拉与界面实际语言显示不一致。
  settings.value.language = i18n.global.locale.value;
}

// —— 语言下拉联动 vue-i18n：选择即生效（无需点保存）——
// setLocale：i18n.global.locale.value = lang + 持久化 localStorage(os.locale)
// + 同步 <html lang>，与顶栏 LanguageSwitcher 共用同一入口，两端保持同步。
watch(
  () => settings.value.language,
  (lang) => setLocale(lang),
);

async function saveSettings(): Promise<void> {
  saving.value = true;
  try {
    window.localStorage.setItem(STORAGE_KEY, JSON.stringify(settings.value));
    toast.success('设置已保存');
  } catch (e) {
    toast.error(`保存失败: ${errMsg(e)}`);
  } finally {
    saving.value = false;
  }
}

// —— API 令牌（服务端启用 NEXOS_ADMIN_TOKEN 时填写，写操作统一携带 Bearer 头）——
const apiToken = ref('');
const apiTokenSaving = ref(false);
const hasApiToken = computed(() => !!apiToken.value.trim());

function saveApiToken(): void {
  apiTokenSaving.value = true;
  try {
    setApiToken(apiToken.value);
    toast.success(hasApiToken.value ? 'API 令牌已保存' : 'API 令牌已清空');
  } catch (e) {
    toast.error(`保存失败: ${errMsg(e)}`);
  } finally {
    apiTokenSaving.value = false;
  }
}

function clearApiToken(): void {
  apiToken.value = '';
  setApiToken('');
  toast.success('API 令牌已清除');
}

// —— IM 联邦（大厅开放开关）已迁入 IM 页（/chat）右上 ⚙️ 设置 面板，2026-08-23 ——

// —— CPU 虚拟化检测（GET /api/v1/system/virt-check）——
const virt = ref<VirtCheckResult | null>(null);
const virtLoading = ref(false);
const virtError = ref('');
const virtDiagnostic = ref('');

async function loadVirtCheck(): Promise<void> {
  virtLoading.value = true;
  virtError.value = '';
  try {
    const raw: any = await api.virtCheck();
    // 后端返回 {diagnostic, is_usable, result: {...}}，拍平 result 到顶层
    if (raw && raw.result && typeof raw.result === 'object') {
      virt.value = { ...raw.result, is_usable: raw.is_usable, diagnostic: raw.diagnostic };
    } else {
      virt.value = raw;
    }
    // 优先用后端 to_user_diagnostic 文本；缺失则前端基于字段生成
    if (virt.value && typeof virt.value.diagnostic === 'string' && virt.value.diagnostic) {
      virtDiagnostic.value = virt.value.diagnostic;
    } else if (virt.value) {
      virtDiagnostic.value = deriveDiagnostic(virt.value);
    } else {
      virtDiagnostic.value = '';
    }
  } catch (e) {
    virt.value = null;
    virtError.value = errMsg(e);
    virtDiagnostic.value = '';
  } finally {
    virtLoading.value = false;
  }
}

/** 前端侧生成用户友好的中文诊断（与 Rust 端 to_user_diagnostic 逻辑近似）。 */
function deriveDiagnostic(v: VirtCheckResult): string {
  const vendor = cpuVendorLabel(v.cpu_vendor);
  if (!v.cpu_has_virt_flags) {
    return `你的 CPU（${vendor}）不支持硬件虚拟化（无 vmx/svm 标志位），无法运行 KVM 虚拟机`;
  }
  if (!v.kvm_device_present) {
    const hint =
      vendor === 'AMD'
        ? '`sudo modprobe kvm kvm_amd`'
        : '`sudo modprobe kvm kvm_intel`';
    return `CPU 支持虚拟化，但 KVM 内核模块未加载。请在 BIOS 中确认已开启 VT-x/AMD-V，然后执行 ${hint}`;
  }
  if (!v.kvm_module_loaded) {
    return `/dev/kvm 存在，但 /proc/modules 未检测到 kvm 模块（可能在容器/沙箱环境）。${vendor} 虚拟化可能仍可用，建议以 /dev/kvm 是否可访问为准。`;
  }
  return `硬件虚拟化就绪（${vendor} /dev/kvm 可用）`;
}

const virtUsable = computed(() => {
  if (!virt.value) return false;
  // 优先采用后端 is_usable() 判定；缺失时按同口径本地推导
  if (typeof virt.value.is_usable === 'boolean') return virt.value.is_usable;
  return !!virt.value.cpu_has_virt_flags && !!virt.value.kvm_device_present;
});

function errMsg(e: unknown): string {
  return e instanceof ApiError || e instanceof Error ? e.message : String(e);
}

onMounted(() => {
  loadSettings();
  loadVirtCheck();
  apiToken.value = getApiToken();
});

/** 重启 os-api（软重启，约 5 秒恢复；轮询健康检查后自动刷新）。 */
async function restartApi(): Promise<void> {
  if (!confirm('确认重启 os-api 服务？（网页将断开数秒后自动恢复）')) return;
  try {
    await post('/api/v1/system/restart');
    for (let i = 0; i < 20; i++) {
      await new Promise(r => setTimeout(r, 1500));
      try { await get('/api/v1/system/healthz'); location.reload(); return; }
      catch { /* 未恢复继续等 */ }
    }
    alert('等待超时——请手动刷新');
  } catch (e) {
    alert('重启失败: ' + (e as Error).message);
  }
}

/** 整机重启（scope=host，重启后需等待开机恢复再刷新）。 */
async function rebootHost(): Promise<void> {
  if (!confirm('整机重启将中断所有服务，恢复取决于开机时长。确认重启本机？')) return;
  try {
    await post('/api/v1/system/restart', { scope: 'host' });
    alert('整机重启已触发——等机器起来后手动刷新页面');
  } catch (e) {
    alert('重启命令失败: ' + (e as Error).message);
  }
}
</script>

<template>
  <div class="settings-page">
    <div class="page-head">
      <div>
        <h2>设置</h2>
        <div class="page-sub">系统名称、语言、时区与虚拟化能力</div>
      </div>
    </div>

    <!-- 系统设置表单 -->
    <div class="card" style="margin-bottom: 16px">
      <div class="card-head">
        <h3>系统信息</h3>
      </div>
      <form class="form" style="margin-top: 6px" @submit.prevent="saveSettings">
        <div class="settings-grid">
          <label>
            系统版本
            <input :value="sysVersion" type="text" readonly style="opacity:.8" />
            <span class="muted small">{{ versionNote }}</span>
          </label>
          <label>
            服务管理
            <span style="display:flex; gap:8px; margin-top:4px">
              <button type="button" class="btn btn-small btn-primary" @click="restartApi">重启 os-api</button>
              <button type="button" class="btn btn-small" @click="rebootHost">整机重启（电源控制）</button>
            </span>
            <span class="muted small">重启 os-api 约 5 秒恢复；整机重启走电源控制页</span>
          </label>
          <label>
            OS 名称
            <input v-model="settings.osName" type="text" placeholder="OS" />
          </label>
          <label>
            语言
            <select v-model="settings.language">
              <option v-for="l in LANGUAGES" :key="l.value" :value="l.value">
                {{ l.label }}
              </option>
            </select>
            <span class="muted small">切换后立即生效（无需保存），与顶栏语言切换同步</span>
          </label>
          <label>
            时区
            <select v-model="settings.timezone">
              <option v-for="tz in COMMON_TIMEZONES" :key="tz" :value="tz">
                {{ tz }}
              </option>
            </select>
          </label>
          <label>
            管理员
            <input
              v-model="settings.admin.name"
              type="text"
              placeholder="admin"
            />
          </label>
          <label>
            管理员邮箱（可选）
            <input
              v-model="settings.admin.email"
              type="email"
              placeholder="admin@example.com"
            />
          </label>
        </div>
        <div class="form-actions">
          <button type="submit" class="btn btn-primary" :disabled="saving">
            保存
          </button>
        </div>
      </form>
    </div>

    <!-- 外观 / 壁纸（6 套预置 CSS 渐变，点击实时切换） -->
    <div class="card" style="margin-bottom: 16px">
      <div class="card-head">
        <h3>外观 / 壁纸</h3>
        <span class="muted small">当前：{{ currentWallpaper.name }}{{ wallpaperIsLight ? '（浅色）' : '' }}</span>
      </div>
      <div class="wallpaper-grid">
        <button
          v-for="wp in wallpapers"
          :key="wp.id"
          type="button"
          class="wallpaper-tile"
          :class="{ active: currentWallpaper.id === wp.id, 'is-light': !wp.textLight }"
          :title="wp.name"
          @click="setWallpaper(wp.id)"
        >
          <span class="wallpaper-swatch" :style="{ background: wp.preview }"></span>
          <span class="wallpaper-name">{{ wp.name }}</span>
          <span v-if="currentWallpaper.id === wp.id" class="wallpaper-check">✓</span>
        </button>
      </div>
    </div>

    <!-- API 令牌（服务端启用 NEXOS_ADMIN_TOKEN 时填写，用于写操作鉴权） -->
    <div class="card" style="margin-bottom: 16px">
      <div class="card-head">
        <h3>API 令牌</h3>
        <span v-if="hasApiToken" class="badge badge-ok">已配置</span>
      </div>
      <form class="form" style="margin-top: 6px" @submit.prevent="saveApiToken">
        <label>
          管理员 Token
          <input
            v-model="apiToken"
            type="password"
            placeholder="sk-..."
            autocomplete="off"
          />
        </label>
        <div class="muted small" style="line-height: 1.6">
          服务端启用 NEXOS_ADMIN_TOKEN 时填写，用于发布/克隆等写操作；token 仅存本浏览器
        </div>
        <div class="form-actions">
          <button type="submit" class="btn btn-primary" :disabled="apiTokenSaving">
            保存
          </button>
          <button type="button" class="btn" @click="clearApiToken">清除</button>
        </div>
      </form>
    </div>

    <!-- IM 联邦（大厅开放开关）已迁入 IM 页（/chat）右上 ⚙️ 设置 面板 -->

    <!-- CPU 虚拟化检测 -->
    <div class="card">
      <div class="card-head">
        <h3>CPU 虚拟化检测</h3>
        <button class="btn btn-small" :disabled="virtLoading" @click="loadVirtCheck">
          ↻ 重新检测
        </button>
      </div>

      <div v-if="virtLoading" class="loading">检测中...</div>
      <div v-else-if="virtError" class="error">检测失败: {{ virtError }}</div>

      <template v-else-if="virt">
        <div
          class="row-gap-sm"
          style="margin: 4px 0 12px"
        >
          <span v-if="virtUsable" class="badge badge-ok">KVM 可用</span>
          <span v-else class="badge badge-err">KVM 不可用</span>
        </div>

        <div class="virt-list">
          <div class="virt-item">
            <span class="virt-key">CPU 厂商</span>
            <span>{{ cpuVendorLabel(virt.cpu_vendor) }}</span>
          </div>
          <div class="virt-item">
            <span class="virt-key">虚拟化标志位 (vmx/svm)</span>
            <span>{{ virt.cpu_has_virt_flags ? '✓ 存在' : '✗ 缺失' }}</span>
          </div>
          <div class="virt-item">
            <span class="virt-key">/dev/kvm 存在</span>
            <span>{{ virt.kvm_device_present ? '✓ 是' : '✗ 否' }}</span>
          </div>
          <div class="virt-item">
            <span class="virt-key">KVM 模块已加载</span>
            <span>{{ virt.kvm_module_loaded ? '✓ 是' : '✗ 否' }}</span>
          </div>
          <div class="virt-item">
            <span class="virt-key">嵌套虚拟化</span>
            <span>{{ nestedVirtLabel(virt.nested_virt) }}</span>
          </div>
        </div>

        <div
          class="muted small"
          style="margin-top: 12px; line-height: 1.6"
        >
          {{ virtDiagnostic }}
        </div>
      </template>
    </div>
  </div>
</template>

<style scoped>
.settings-page {
  padding: 20px 24px;
  display: flex;
  flex-direction: column;
  gap: 16px;
}
/* ============================================================
   壁纸选择网格（6 套预置色块）
   ============================================================ */
.wallpaper-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(140px, 1fr));
  gap: 12px;
  margin-top: 8px;
}
.wallpaper-tile {
  position: relative;
  display: flex;
  flex-direction: column;
  gap: 8px;
  padding: 8px;
  background: var(--bg-elev, #f7f7f7);
  border: 2px solid var(--border, #d9d9d9);
  border-radius: var(--radius-md, 8px);
  cursor: pointer;
  transition: border-color 0.14s ease, box-shadow 0.14s ease, transform 0.12s ease;
  font-family: inherit;
}
.wallpaper-tile:hover {
  transform: translateY(-2px);
  box-shadow: var(--shadow-lg, 0 4px 16px rgba(0, 0, 0, 0.12));
}
.wallpaper-tile.active {
  border-color: var(--accent, #e95420);
  box-shadow: 0 0 0 3px var(--accent-soft, rgba(233, 84, 32, 0.12));
}
.wallpaper-swatch {
  display: block;
  width: 100%;
  height: 60px;
  border-radius: var(--radius-sm, 6px);
  border: 1px solid rgba(0, 0, 0, 0.08);
}
.wallpaper-name {
  font-size: 12.5px;
  font-weight: 600;
  color: var(--text, #2b2b2b);
  text-align: center;
}
.wallpaper-check {
  position: absolute;
  top: 6px;
  right: 6px;
  width: 20px;
  height: 20px;
  border-radius: 50%;
  background: var(--accent, #e95420);
  color: #fff;
  font-size: 12px;
  font-weight: 700;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  line-height: 1;
}
</style>
