<script setup lang="ts">
// =============================================================================
// Update.vue —— 更新（系统更新通道）
//
// 4 区：当前版本卡（版本号 + 通道徽章 + A/B 槽位小图示）/ 更新通道四选一 /
// 检查更新 → 可用版本列表（每行「应用更新」→ 任务进度条轮询）/ 更新历史。
// 后端：/api/v1/update/*（UpdateRouteHandler，已在线）。
//
// 设计：Ubuntu Yaru 风 .card / .page-head（同 Provisioning.vue 的 zone/card 体系）。
// 开发期语义：apply 任务推进到 writing 后标记"通道已预留"（note 展示），
// 真实镜像下载/写槽等待镜像源接入（docs/UPDATE_APP.md）。
// =============================================================================
import { computed, onBeforeUnmount, onMounted, ref } from 'vue';
import {
  endpoints,
  type UpdateAvailableItem,
  type UpdateChannel,
  type UpdateChannelInfo,
  type UpdateStatusResp,
  type UpdateTask,
} from '@/api/client';

// =============================================================================
// 全局消息 / 加载态
// =============================================================================
const msg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);
function friendlyError(e: unknown): string {
  const m = e instanceof Error ? e.message : String(e);
  if (/404|405|not found|method not allowed/i.test(m)) {
    return '后端尚未实现该更新接口';
  }
  return m;
}

// =============================================================================
// 区1：当前版本 + 槽位视图（GET /status）
// =============================================================================
const status = ref<UpdateStatusResp | null>(null);
const statusLoading = ref(false);
const statusError = ref('');

const channelLabel = computed<string>(() => {
  const id = status.value?.channel;
  const hit = channels.value.find((c) => c.id === id);
  return hit ? hit.name : (id ?? '—');
});

async function loadStatus(): Promise<void> {
  statusLoading.value = true;
  statusError.value = '';
  try {
    status.value = await endpoints.updateStatus();
  } catch (e) {
    status.value = null;
    statusError.value = friendlyError(e);
  } finally {
    statusLoading.value = false;
  }
}

// =============================================================================
// 区2：更新通道四选一（GET /channels + POST /channel）
// =============================================================================
const channels = ref<UpdateChannelInfo[]>([]);
const currentChannel = ref<UpdateChannel | ''>('');
const channelsLoading = ref(false);
const switchingChannel = ref<UpdateChannel | ''>('');

async function loadChannels(): Promise<void> {
  channelsLoading.value = true;
  try {
    const resp = await endpoints.updateChannels();
    channels.value = resp.channels;
    currentChannel.value = resp.current;
  } catch (e) {
    msg.value = { kind: 'err', text: '通道加载失败：' + friendlyError(e) };
  } finally {
    channelsLoading.value = false;
  }
}

async function pickChannel(id: UpdateChannel): Promise<void> {
  if (id === currentChannel.value || switchingChannel.value) return;
  switchingChannel.value = id;
  msg.value = null;
  try {
    await endpoints.updateSetChannel(id);
    currentChannel.value = id;
    msg.value = { kind: 'ok', text: `已切换到「${channels.value.find((c) => c.id === id)?.name ?? id}」通道（已持久化）` };
    // 通道变了，既有清单作废
    available.value = [];
    void loadStatus();
  } catch (e) {
    msg.value = { kind: 'err', text: '切换失败：' + friendlyError(e) };
  } finally {
    switchingChannel.value = '';
  }
}

// =============================================================================
// 区3：检查更新 → 可用版本列表 → 应用任务（POST /check → /apply → 轮询 tasks/:id）
// =============================================================================
const checking = ref(false);
const available = ref<UpdateAvailableItem[]>([]);
const repoMode = ref<'local' | 'remote' | 'none' | null>(null);
const repoUrl = ref<string | null>(null);
const repoDesc = ref('');
const lastCheckAt = ref('');
const applyingVersion = ref('');

// 更新源三态文案（check 后展示）：本地仓库模式 / 远端 git 模式（显示 URL）/
// 均不可达。后端两级源解析链：本地 NexHub 裸仓库优先，缺失时走
// NEXOS_UPDATE_REPO_URL 的 git ls-remote 网络查询。
const sourceLine = computed<string | null>(() => {
  if (repoMode.value === null) return null;
  if (repoMode.value === 'local') {
    return `更新源（本地仓库）：${repoDesc.value} —— 联邦 auto-pull 本地副本`;
  }
  if (repoMode.value === 'remote') {
    return `更新源（远端 git）：${repoDesc.value} —— git ls-remote --tags 网络查询`;
  }
  return repoUrl.value
    ? `更新源不可达：本地 ${repoDesc.value} 与远端 ${repoUrl.value} 均不可达——降级为空清单`
    : `更新源不可达：本地 ${repoDesc.value} 不可达且未配置远端更新源（NEXOS_UPDATE_REPO_URL）——降级为空清单`;
});

async function checkUpdates(): Promise<void> {
  checking.value = true;
  msg.value = null;
  try {
    const resp = await endpoints.updateCheck();
    available.value = resp.available;
    repoMode.value = resp.repo_mode;
    repoUrl.value = resp.repo_url;
    repoDesc.value = resp.repo;
    lastCheckAt.value = resp.checked_at;
    if (!resp.repo_reachable) {
      msg.value = {
        kind: 'info',
        text: repoUrl.value
          ? `更新源均不可达（本地 ${resp.repo} / 远端 ${resp.repo_url}），降级为空清单——检查远端节点的 git HTTP 通道`
          : `更新源不可达（本地 ${resp.repo}），降级为空清单——未配置远端更新源；重装新版 install.sh 可自动写入 NEXOS_UPDATE_REPO_URL（指向安装源节点 git 通道）`,
      };
    } else if (resp.available.length === 0) {
      msg.value = { kind: 'ok', text: `当前已是「${resp.channel}」通道最新版本（${resp.current_version}）` };
    }
    void loadStatus();
  } catch (e) {
    msg.value = { kind: 'err', text: '检查失败：' + friendlyError(e) };
  } finally {
    checking.value = false;
  }
}

// —— 任务进度（轮询推进状态机）——
const activeTask = ref<UpdateTask | null>(null);
let pollTimer: ReturnType<typeof setInterval> | null = null;

function stopPolling(): void {
  if (pollTimer !== null) {
    clearInterval(pollTimer);
    pollTimer = null;
  }
}

async function pollTask(id: string): Promise<void> {
  try {
    const t = await endpoints.updateTask(id);
    activeTask.value = t;
    if (t.status === 'done' || t.status === 'failed') {
      stopPolling();
      if (t.status === 'done') {
        msg.value = { kind: 'ok', text: `更新任务 ${t.id} 已完成（通道预留推进到 done；真实写槽/重启待镜像源接入）` };
      } else {
        msg.value = { kind: 'err', text: `更新任务失败：${t.error ?? '未知原因'}` };
      }
      void loadHistory();
    }
  } catch {
    // 单次轮询失败不断流程（网络抖动等）；下一轮继续
  }
}

async function applyUpdate(item: UpdateAvailableItem): Promise<void> {
  if (applyingVersion.value || pollTimer !== null) return;
  if (!window.confirm(`确定应用更新到 ${item.version}（tag ${item.tag}）？将写入槽 B（本期为通道预留推进，不执行真实写槽）。`)) return;
  applyingVersion.value = item.version;
  msg.value = null;
  try {
    const task = await endpoints.updateApply(item.version);
    activeTask.value = task;
    stopPolling();
    pollTimer = setInterval(() => void pollTask(task.id), 1500);
    void pollTask(task.id);
  } catch (e) {
    msg.value = { kind: 'err', text: '应用失败：' + friendlyError(e) };
    applyingVersion.value = '';
  } finally {
    applyingVersion.value = '';
  }
}

const taskPhaseText: Record<string, string> = {
  pending: '排队中',
  downloading: '下载中',
  verifying: '校验中',
  writing: '写入槽位',
  reboot_pending: '等待重启',
  done: '已完成',
  failed: '失败',
};

// =============================================================================
// 区4：更新历史（GET /history）
// =============================================================================
const history = ref<UpdateTask[]>([]);
const historyLoading = ref(false);

async function loadHistory(): Promise<void> {
  historyLoading.value = true;
  try {
    history.value = await endpoints.updateHistory();
  } catch {
    history.value = [];
  } finally {
    historyLoading.value = false;
  }
}

// =============================================================================
// 汇总刷新
// =============================================================================
const refreshing = ref(false);
async function refreshAll(): Promise<void> {
  refreshing.value = true;
  try {
    await Promise.all([loadStatus(), loadChannels(), loadHistory()]);
  } finally {
    refreshing.value = false;
  }
}

onMounted(() => {
  void refreshAll();
});
onBeforeUnmount(stopPolling);
</script>

<template>
  <div class="update-page">
    <div class="page-head">
      <div>
        <h2 class="page-title">更新</h2>
        <div class="page-sub muted">版本 · 通道 · A/B 槽位 · 更新任务</div>
      </div>
      <div class="head-actions">
        <button class="btn btn-small" :disabled="refreshing" @click="refreshAll">
          <span class="spin" :class="{ spinning: refreshing }" aria-hidden="true">↻</span>
          刷新
        </button>
      </div>
    </div>

    <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>

    <!-- =================== 区1 当前版本卡 =================== -->
    <section class="card version-card">
      <div v-if="statusError" class="error-box">状态加载失败：{{ statusError }}</div>
      <template v-else-if="status">
        <div class="version-main">
          <div class="version-number mono">{{ status.current_version }}</div>
          <div class="version-meta">
            <span class="pill pill-blue">{{ channelLabel }}通道</span>
            <span
              v-if="status.last_check"
              class="muted small"
            >上次检查：{{ status.last_check }}</span>
            <span v-else class="muted small">尚未检查过更新</span>
          </div>
          <div v-if="status.pending_updates.length" class="version-pending muted small">
            待应用 {{ status.pending_updates.length }} 个版本（最近：
            <span class="mono">{{ status.pending_updates[0].version }}</span>）——点击下方「检查更新」刷新
          </div>
        </div>
        <!-- A/B 槽位小图示 -->
        <div class="slots-view">
          <div class="slot-chip" :class="{ 'slot-active': status.active_slot === 'a' }">
            <div class="slot-name">槽 A</div>
            <div class="slot-status">
              <span class="pill" :class="status.active_slot === 'a' ? 'pill-ok' : 'pill-muted'">
                {{ status.slot_a.status === 'active' ? '运行中' : status.slot_a.status }}
              </span>
            </div>
            <div class="slot-version mono">{{ status.slot_a.version ?? '（空）' }}</div>
          </div>
          <div class="slot-arrow" aria-hidden="true">⇄</div>
          <div class="slot-chip" :class="{ 'slot-active': status.active_slot === 'b' }">
            <div class="slot-name">槽 B</div>
            <div class="slot-status">
              <span
                class="pill"
                :class="status.writable_slot === 'b' ? 'pill-warn' : 'pill-muted'"
              >
                {{ status.slot_b.version ? '备用' : '可写入' }}
              </span>
            </div>
            <div class="slot-version mono">{{ status.slot_b.version ?? '（空）' }}</div>
          </div>
          <div class="slots-note muted small">
            A/B 双槽：更新写入备用槽，激活后切换；失败可回滚。
          </div>
        </div>
      </template>
      <div v-else class="muted">加载中…</div>
    </section>

    <!-- =================== 区2 更新通道四选一 =================== -->
    <section class="card channel-card">
      <div class="panel-head">
        <h3>更新通道</h3>
        <span v-if="channelsLoading" class="muted small">加载中…</span>
      </div>
      <div class="channel-grid">
        <button
          v-for="c in channels"
          :key="c.id"
          class="channel-option"
          :class="{ selected: c.id === currentChannel }"
          :disabled="switchingChannel !== ''"
          @click="pickChannel(c.id)"
        >
          <span class="channel-name">{{ c.name }}</span>
          <span class="channel-id mono muted small">{{ c.id }}</span>
          <span class="channel-desc">{{ c.description }}</span>
          <span
            v-if="c.id === currentChannel"
            class="pill pill-ok channel-current"
          >当前</span>
        </button>
      </div>
    </section>

    <!-- =================== 区3 检查更新 + 可用版本 =================== -->
    <section class="card check-card">
      <div class="panel-head">
        <h3>可用更新</h3>
        <button class="btn btn-primary" :disabled="checking" @click="checkUpdates">
          {{ checking ? '检查中…' : '检查更新' }}
        </button>
      </div>
      <div v-if="sourceLine !== null && !checking" class="muted small">
        <span class="mono">{{ sourceLine }}</span>
      </div>

      <!-- 进行中的任务进度条 -->
      <div v-if="activeTask" class="task-progress">
        <div class="task-progress-head">
          <span class="mono">{{ activeTask.id }}</span>
          <span class="pill" :class="activeTask.status === 'failed' ? 'pill-err' : activeTask.status === 'done' ? 'pill-ok' : 'pill-blue'">
            {{ taskPhaseText[activeTask.status] ?? activeTask.status }}
          </span>
          <span class="muted small">目标 <span class="mono">{{ activeTask.version }}</span> → 写入槽 {{ activeTask.slot_target.toUpperCase() }}</span>
        </div>
        <div class="progress-track">
          <div
            class="progress-fill"
            :class="{ failed: activeTask.status === 'failed' }"
            :style="{ width: `${activeTask.progress}%` }"
          ></div>
        </div>
        <div class="muted small">{{ activeTask.progress }}% · {{ taskPhaseText[activeTask.status] ?? activeTask.status }}（轮询 tasks/:id 推进状态机）</div>
        <div v-if="activeTask.note" class="task-note">{{ activeTask.note }}</div>
        <div v-if="activeTask.error" class="error-text">{{ activeTask.error }}</div>
      </div>

      <div v-if="available.length" class="available-list">
        <div v-for="u in available" :key="u.tag" class="available-row">
          <div class="avail-version mono">{{ u.version }}</div>
          <div class="avail-meta">
            <span class="pill" :class="u.channel === 'stable' ? 'pill-ok' : u.channel === 'beta' ? 'pill-purple' : 'pill-warn'">
              {{ u.channel === 'prerelease' ? '预发布' : u.channel }}
            </span>
            <span class="mono muted small">{{ u.tag }}</span>
            <span v-if="u.created_at" class="muted small">{{ u.created_at }}</span>
          </div>
          <button
            class="btn btn-small btn-primary"
            :disabled="applyingVersion !== '' || pollTimer !== null"
            @click="applyUpdate(u)"
          >应用更新</button>
        </div>
      </div>
      <div v-else-if="!checking" class="muted">
        暂无可用版本——点击「检查更新」从 NexHub 发版 tag 读取（按当前通道过滤、仅列新于
        <span class="mono">{{ status?.current_version ?? '…' }}</span> 的版本）。
      </div>
    </section>

    <!-- =================== 区4 更新历史 =================== -->
    <section class="card history-card">
      <div class="panel-head">
        <h3>更新历史</h3>
        <span v-if="historyLoading" class="muted small">加载中…</span>
      </div>
      <div v-if="history.length" class="history-list">
        <div v-for="t in history" :key="t.id" class="history-row">
          <span class="mono">{{ t.version }}</span>
          <span class="pill" :class="t.status === 'done' ? 'pill-ok' : 'pill-warn'">
            {{ t.status === 'done' ? '已应用' : '待重启' }}
          </span>
          <span class="muted small">{{ t.created_at }}</span>
          <span class="muted small">通道 {{ t.channel }} · 槽 {{ t.slot_target.toUpperCase() }}</span>
        </div>
      </div>
      <div v-else class="muted">暂无历史——已应用（done / 待重启）任务会出现在这里。</div>
    </section>
  </div>
</template>

<style scoped>
.update-page {
  padding: 20px 24px;
  display: flex;
  flex-direction: column;
  gap: 18px;
}
.page-head { display: flex; justify-content: space-between; align-items: center; gap: 12px; flex-wrap: wrap; }
.page-title { font-size: 22px; font-weight: 700; color: var(--text, #2B2B2B); letter-spacing: -0.02em; }
.page-sub { margin-top: 4px; font-size: 13px; }
.head-actions { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.muted { color: var(--text-muted, #5E5C5F); }
.small { font-size: 12px; }
.mono { font-family: var(--mono); }

.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
  padding: 18px 20px;
  display: flex;
  flex-direction: column;
  gap: 14px;
}
.panel-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.panel-head h3 { font-size: 16px; font-weight: 600; color: var(--text, #2B2B2B); }

/* —— 区1 当前版本卡 —— */
.version-card { flex-direction: row; align-items: center; justify-content: space-between; gap: 20px; flex-wrap: wrap; }
.version-main { display: flex; flex-direction: column; gap: 6px; min-width: 220px; }
.version-number { font-size: 40px; font-weight: 700; color: var(--text, #2B2B2B); letter-spacing: -0.02em; line-height: 1.1; }
.version-meta { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.version-pending { line-height: 1.6; }
.slots-view { display: flex; align-items: center; gap: 12px; flex-wrap: wrap; }
.slot-chip {
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  padding: 10px 16px;
  display: flex;
  flex-direction: column;
  gap: 4px;
  min-width: 130px;
  background: var(--bg-card, #fff);
}
.slot-chip.slot-active { border-color: #0E8420; box-shadow: 0 0 0 3px rgba(14, 132, 32, 0.12); }
.slot-name { font-size: 12px; font-weight: 700; text-transform: uppercase; letter-spacing: 0.6px; color: var(--text-muted, #5E5C5F); }
.slot-status { display: flex; }
.slot-version { font-size: 15px; color: var(--text, #2B2B2B); }
.slot-arrow { font-size: 20px; color: var(--text-muted, #5E5C5F); }
.slots-note { flex-basis: 100%; }

/* —— 区2 通道四选一 —— */
.channel-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(200px, 1fr)); gap: 12px; }
.channel-option {
  position: relative;
  text-align: left;
  border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-md, 12px);
  background: var(--bg-card, #fff);
  padding: 14px 16px;
  display: flex;
  flex-direction: column;
  gap: 4px;
  cursor: pointer;
  font-family: inherit;
  transition: border-color 0.15s ease, box-shadow 0.15s ease;
}
.channel-option:hover:not(:disabled) { border-color: var(--accent, #E95420); }
.channel-option.selected { border-color: var(--accent, #E95420); box-shadow: 0 0 0 3px rgba(233, 84, 32, 0.15); }
.channel-option:disabled { opacity: 0.6; cursor: not-allowed; }
.channel-name { font-size: 15px; font-weight: 600; color: var(--text, #2B2B2B); }
.channel-desc { font-size: 12.5px; color: var(--text-muted, #5E5C5F); line-height: 1.6; }
.channel-current { position: absolute; top: 10px; right: 10px; }

/* —— 区3 检查更新 —— */
.available-list { display: flex; flex-direction: column; gap: 8px; }
.available-row {
  display: flex;
  align-items: center;
  gap: 14px;
  padding: 10px 14px;
  border: 1px solid var(--border-soft, #EDEDED);
  border-radius: var(--radius-sm, 8px);
  flex-wrap: wrap;
}
.avail-version { font-size: 17px; font-weight: 700; color: var(--text, #2B2B2B); min-width: 90px; }
.avail-meta { display: flex; align-items: center; gap: 10px; flex: 1; flex-wrap: wrap; }
.available-row .btn { margin-left: auto; }

/* 任务进度 */
.task-progress { display: flex; flex-direction: column; gap: 8px; padding: 12px 14px; border: 1px dashed var(--border, #d1d5db); border-radius: var(--radius-sm, 8px); }
.task-progress-head { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.progress-track { height: 10px; border-radius: 999px; background: #f3f4f6; overflow: hidden; }
.progress-fill { height: 100%; border-radius: 999px; background: linear-gradient(90deg, #E95420, #F99B11); transition: width 0.4s ease; }
.progress-fill.failed { background: #b91c1c; }
.task-note { font-size: 12.5px; color: #92600a; background: #fef3c7; border-radius: var(--radius-sm, 8px); padding: 8px 12px; line-height: 1.6; }
.error-text { color: #b91c1c; font-size: 12.5px; }

/* —— 区4 历史 —— */
.history-list { display: flex; flex-direction: column; gap: 8px; }
.history-row {
  display: flex; align-items: center; gap: 12px; flex-wrap: wrap;
  padding: 8px 14px; border-bottom: 1px solid var(--border-soft, #EDEDED);
}
.history-row:last-child { border-bottom: none; }

/* —— 通用 —— */
.form-msg { font-size: 13px; padding: 2px 0; }
.form-msg.is-err { color: #b91c1c; }
.form-msg.is-ok { color: #15803d; }
.form-msg.is-info { color: var(--text-muted, #6b7280); }
.error-box { color: #b91c1c; background: #fee2e2; border: 1px solid rgba(185, 28, 28, 0.2); padding: 10px 14px; border-radius: var(--radius-sm, 8px); font-size: 13px; }
.pill { display: inline-block; padding: 2px 10px; border-radius: var(--radius-pill, 20px); font-size: 12px; font-weight: 600; }
.pill-ok { color: #15803d; background: #dcfce7; }
.pill-blue { color: #C7421A; background: #dbeafe; }
.pill-err { color: #b91c1c; background: #fee2e2; }
.pill-muted { color: #6b7280; background: #f3f4f6; }
.pill-warn { color: #92600a; background: #fef3c7; }
.pill-purple { color: #7c3aed; background: #ede9fe; }
.btn {
  padding: 6px 14px; border-radius: var(--radius-sm, 8px);
  border: 1px solid var(--border, #d1d5db); background: var(--bg-card, #fff);
  color: var(--text, #2B2B2B); font-size: 13px; cursor: pointer; font-family: inherit;
  transition: background 0.15s ease;
}
.btn:hover:not(:disabled) { background: rgba(0, 0, 0, 0.04); }
.btn:disabled { opacity: 0.5; cursor: not-allowed; }
.btn-small { padding: 4px 10px; font-size: 12.5px; }
.btn-primary { background: var(--accent, #E95420); color: #fff; border-color: var(--accent, #E95420); }
.btn-primary:hover:not(:disabled) { background: var(--accent-hi, #0077ed); }
.spin { display: inline-block; font-size: 14px; line-height: 1; }
.spin.spinning { animation: spin 0.8s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }

@media (max-width: 720px) {
  .update-page { padding: 16px; }
  .version-card { flex-direction: column; align-items: stretch; }
  .channel-grid { grid-template-columns: 1fr; }
  .available-row .btn { margin-left: 0; }
}
</style>
