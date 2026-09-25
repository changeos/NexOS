<script setup lang="ts">
// =============================================================================
// RepoReleasesTab —— 仓库详情 Releases Tab（v0.1.50 方案 §top3 Release 二进制附件）。
//
// Release 卡（大厅 hub_releases：tag + 标题 + 说明，创建/删除仅 admin）+
// 每卡资产列表（仓级 /releases/:tag/assets：上传/删除=owner/admin、下载公开
// octet-stream 直链）。上传走流式通道（v0.1.53：?stream=1 + octet-stream
// 直收——fetch 以 File 为 body，浏览器从磁盘流式发送，不再 FileReader→base64，
// 峰值内存省一倍+；后端逐块落盘 + sha256）。下载端点同路径 octet-stream
// 流式直挂（Range 断点续传），<a download> 直链语义不变。
// =============================================================================
import { ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import { endpoints } from '@/api/client';
import type { LobbyRelease, ReleaseAsset } from '@/api/client';
import { useNexhub } from '@/views/nexhub/context';
import { errMsg, formatBytes, formatDate } from '@/views/nexhub/model';
import { collabWriteErr, useCollabIdentity } from '@/views/nexhub/collab';
import NexhubConfirm from '@/views/nexhub/components/NexhubConfirm.vue';

const props = defineProps<{
  repoName: string;
}>();

const { t } = useI18n();
const ctx = useNexhub();

// 身份 / 权限：附件管理=owner/admin（同 merge）；release 创建/删除=仅 admin（大厅契约）。
const idctx = useCollabIdentity(() => ctx.lobbyEntries.value);

/** 单个 release 附件上限（解码后 100MB，与后端 MAX_ASSET_BYTES 一致）。 */
const MAX_ASSET_BYTES = 100 * 1024 * 1024;

interface ReleaseCard {
  release: LobbyRelease;
  assets: ReleaseAsset[];
  assetsLoading: boolean;
}

const releases = ref<ReleaseCard[]>([]);
const loading = ref(false);

/** 发版对话框（仅 admin）。 */
const showCreate = ref(false);
const createForm = ref({ tag: '', title: '', notes: '' });
const creating = ref(false);

/** 附件上传对话框（挂在某 release 卡上）。 */
const uploadFor = ref<string | null>(null);
const uploadName = ref('');
const uploadFile = ref<File | null>(null);
const uploading = ref(false);

/** 附件删除确认（站内弹窗）。 */
const deletePending = ref<{ tag: string; asset: ReleaseAsset } | null>(null);

/** 附件分卡懒加载（release 列表公开端点不带 assets，逐卡拉清单）。 */
async function loadAssets(card: ReleaseCard): Promise<void> {
  card.assetsLoading = true;
  try {
    const r = await endpoints.codeRepoReleaseAssets(props.repoName.trim(), card.release.tag);
    card.assets = r.assets ?? [];
  } catch {
    // release 行被并发删除等 → 清单降级为空（卡片仍可见）
    card.assets = [];
  } finally {
    card.assetsLoading = false;
  }
}

async function loadReleases(): Promise<void> {
  const repo = props.repoName.trim();
  if (!repo) return;
  loading.value = true;
  try {
    const list = await endpoints.nexhubLobbyReleases(repo);
    const cards: ReleaseCard[] = (list ?? []).map((release) => ({
      release,
      assets: [],
      assetsLoading: false,
    }));
    releases.value = cards;
    await Promise.all(cards.map((c) => loadAssets(c)));
  } catch (e) {
    ctx.showMsg('error', `${t('nexhub.releases.loadFailed')}: ${errMsg(e)}`);
    releases.value = [];
  } finally {
    loading.value = false;
  }
}

/** 创建 release（仅 admin；tag 打在默认分支头）。 */
async function createRelease(): Promise<void> {
  const repo = props.repoName.trim();
  const tag = createForm.value.tag.trim();
  if (!tag) {
    ctx.showMsg('error', t('nexhub.releases.tagRequired'));
    return;
  }
  creating.value = true;
  ctx.clearMsg();
  try {
    const opts = await idctx.requireNexhubOpts();
    await endpoints.nexhubLobbyReleaseCreate(
      repo,
      {
        tag,
        title: createForm.value.title.trim() || undefined,
        notes: createForm.value.notes.trim() || undefined,
      },
      opts,
    );
    ctx.showMsg('ok', t('nexhub.releases.created', { tag }));
    showCreate.value = false;
    createForm.value = { tag: '', title: '', notes: '' };
    await loadReleases();
  } catch (e) {
    ctx.showMsg('error', collabWriteErr(t('nexhub.releases.createAction'), e));
  } finally {
    creating.value = false;
  }
}

/** 删除 release（仅 admin；库行 + git tag + 附件清单一并删除）。 */
async function deleteRelease(card: ReleaseCard): Promise<void> {
  const repo = props.repoName.trim();
  ctx.actionLoading.value = true;
  ctx.clearMsg();
  try {
    const opts = await idctx.requireNexhubOpts();
    await endpoints.nexhubLobbyReleaseDelete(repo, card.release.tag, opts);
    ctx.showMsg('ok', t('nexhub.releases.deleted', { tag: card.release.tag }));
    await loadReleases();
  } catch (e) {
    ctx.showMsg('error', collabWriteErr(t('nexhub.releases.deleteAction'), e));
  } finally {
    ctx.actionLoading.value = false;
  }
}

function openUpload(tag: string): void {
  uploadFor.value = tag;
  uploadName.value = '';
  uploadFile.value = null;
}

/** 选文件 → 预填附件名（可改）。 */
function onFilePicked(e: Event): void {
  const input = e.target as HTMLInputElement;
  uploadFile.value = input.files?.[0] ?? null;
  if (uploadFile.value && !uploadName.value.trim()) {
    uploadName.value = uploadFile.value.name;
  }
}

/** 上传附件（owner/admin）：File 直传流式通道（?stream=1 octet-stream——
 * 浏览器从磁盘流式发送，不再 FileReader→base64；fetch 无原生上传进度，
 * 上传中 spinner 即反馈）。 */
async function uploadAsset(): Promise<void> {
  const repo = props.repoName.trim();
  const tag = uploadFor.value;
  const file = uploadFile.value;
  const name = uploadName.value.trim();
  if (!repo || !tag || !file) return;
  if (!name) {
    ctx.showMsg('error', t('nexhub.releases.assetNameRequired'));
    return;
  }
  if (file.size > MAX_ASSET_BYTES) {
    ctx.showMsg('error', t('nexhub.releases.assetTooLarge'));
    return;
  }
  uploading.value = true;
  ctx.clearMsg();
  try {
    const opts = await idctx.requireNexhubOpts();
    const r = await endpoints.codeRepoReleaseAssetUploadStream(repo, tag, name, file, opts);
    ctx.showMsg('ok', t('nexhub.releases.assetUploaded', { name: r.asset.name }));
    uploadFor.value = null;
    const card = releases.value.find((c) => c.release.tag === tag);
    if (card) await loadAssets(card);
  } catch (e) {
    ctx.showMsg('error', collabWriteErr(t('nexhub.releases.assetUploadAction'), e));
  } finally {
    uploading.value = false;
  }
}

/** 附件直链（octet-stream 直传，浏览器直接下载）。 */
function assetUrl(a: ReleaseAsset): string {
  return endpoints.codeRepoReleaseAssetUrl(props.repoName.trim(), a.release_tag, a.id);
}

/** 删除附件（owner/admin）。 */
async function doDeleteAsset(): Promise<void> {
  const repo = props.repoName.trim();
  const pending = deletePending.value;
  deletePending.value = null;
  if (!repo || !pending) return;
  ctx.actionLoading.value = true;
  ctx.clearMsg();
  try {
    const opts = await idctx.requireNexhubOpts();
    await endpoints.codeRepoReleaseAssetDelete(repo, pending.tag, pending.asset.id, opts);
    ctx.showMsg('ok', t('nexhub.releases.assetDeleted', { name: pending.asset.name }));
    const card = releases.value.find((c) => c.release.tag === pending.tag);
    if (card) await loadAssets(card);
  } catch (e) {
    ctx.showMsg('error', collabWriteErr(t('nexhub.releases.assetDeleteAction'), e));
  } finally {
    ctx.actionLoading.value = false;
  }
}

// 仓库切换：重载
watch(
  () => props.repoName,
  () => void loadReleases(),
  { immediate: true },
);
</script>

<template>
  <section class="releases-tab">
    <div class="browser-toolbar">
      <button class="btn btn-small" type="button" :disabled="loading" @click="loadReleases">
        <span class="spin" :class="{ spinning: loading }" aria-hidden="true">↻</span>
        {{ t('nexhub.common.refresh') }}
      </button>
      <button
        v-if="idctx.hasAdminToken.value"
        class="btn btn-small btn-primary"
        type="button"
        @click="showCreate = true"
      >+ {{ t('nexhub.releases.create') }}</button>
    </div>

    <p class="muted small hint">{{ t('nexhub.releases.hint') }}</p>

    <div v-if="loading" class="card empty-card">{{ t('common.loading') }}</div>
    <div v-else-if="releases.length === 0" class="card empty-card">{{ t('nexhub.releases.empty') }}</div>

    <div v-else class="release-list">
      <div v-for="card in releases" :key="card.release.id" class="card release-card">
        <div class="release-head">
          <span class="release-tag">🏷 {{ card.release.tag }}</span>
          <strong class="release-title">{{ card.release.title }}</strong>
          <span class="muted small">{{ formatDate(card.release.created_at) }}</span>
          <span class="head-spacer" />
          <button
            v-if="idctx.canMergePull(props.repoName)"
            class="btn btn-small"
            type="button"
            @click="openUpload(card.release.tag)"
          >⬆ {{ t('nexhub.releases.assetUploadBtn') }}</button>
          <button
            v-if="idctx.hasAdminToken.value"
            class="btn btn-small btn-danger"
            type="button"
            @click="deleteRelease(card)"
          >{{ t('nexhub.releases.deleteBtn') }}</button>
        </div>
        <p v-if="card.release.notes" class="release-notes muted">{{ card.release.notes }}</p>

        <!-- 资产清单 -->
        <div class="asset-list">
          <div v-if="card.assetsLoading" class="muted small">{{ t('common.loading') }}</div>
          <div v-else-if="card.assets.length === 0" class="muted small asset-empty">
            {{ t('nexhub.releases.assetsEmpty') }}
          </div>
          <div v-for="a in card.assets" :key="a.id" class="asset-row">
            <span class="asset-name">📦 {{ a.name }}</span>
            <code class="asset-sha" :title="t('nexhub.releases.assetShaTitle')">{{ a.sha256.slice(0, 12) }}</code>
            <span class="muted small">{{ formatBytes(a.size) }} · {{ formatDate(a.created_at) }}</span>
            <span class="head-spacer" />
            <a class="btn btn-small" :href="assetUrl(a)" :download="a.name">
              ⬇ {{ t('nexhub.releases.assetDownload') }}
            </a>
            <button
              v-if="idctx.canMergePull(props.repoName)"
              class="btn btn-small btn-danger"
              type="button"
              @click="deletePending = { tag: card.release.tag, asset: a }"
            >✕</button>
          </div>
        </div>
      </div>
    </div>

    <!-- 发版对话框（仅 admin） -->
    <div v-if="showCreate" class="modal-overlay" @click.self="showCreate = false">
      <div class="card modal-card">
        <div class="modal-head">
          <h3 class="modal-title">{{ t('nexhub.releases.createTitle', { repo: props.repoName }) }}</h3>
          <button class="btn btn-small btn-ghost" type="button" @click="showCreate = false">✕</button>
        </div>
        <div class="form-row">
          <span class="form-hint muted small">{{ t('nexhub.releases.createHint') }}</span>
        </div>
        <div class="form-row">
          <label class="form-label" for="rel-tag">{{ t('nexhub.releases.tagLabel') }} *</label>
          <input
            id="rel-tag"
            v-model="createForm.tag"
            class="search-input"
            placeholder="v1.0.0"
          />
        </div>
        <div class="form-row">
          <label class="form-label" for="rel-title">{{ t('nexhub.releases.titleLabel') }}</label>
          <input id="rel-title" v-model="createForm.title" class="search-input" />
        </div>
        <div class="form-row">
          <label class="form-label" for="rel-notes">{{ t('nexhub.releases.notesLabel') }}</label>
          <textarea
            id="rel-notes"
            v-model="createForm.notes"
            class="search-input notes-input"
            rows="4"
          ></textarea>
        </div>
        <div class="modal-actions">
          <button class="btn btn-small" type="button" @click="showCreate = false">{{ t('common.cancel') }}</button>
          <button
            class="btn btn-small btn-primary"
            type="button"
            :disabled="creating || ctx.actionLoading.value"
            @click="createRelease"
          >{{ t('nexhub.releases.create') }}</button>
        </div>
      </div>
    </div>

    <!-- 附件上传对话框（owner/admin） -->
    <div v-if="uploadFor" class="modal-overlay" @click.self="uploadFor = null">
      <div class="card modal-card">
        <div class="modal-head">
          <h3 class="modal-title">{{ t('nexhub.releases.assetUploadTitle', { tag: uploadFor }) }}</h3>
          <button class="btn btn-small btn-ghost" type="button" @click="uploadFor = null">✕</button>
        </div>
        <div class="form-row">
          <span class="form-hint muted small">{{ t('nexhub.releases.assetUploadHint') }}</span>
        </div>
        <div class="form-row">
          <label class="form-label" for="asset-file">{{ t('nexhub.releases.assetFileLabel') }}</label>
          <input
            id="asset-file"
            class="search-input"
            type="file"
            @change="onFilePicked"
          />
        </div>
        <div class="form-row">
          <label class="form-label" for="asset-name">{{ t('nexhub.releases.assetNameLabel') }}</label>
          <input id="asset-name" v-model="uploadName" class="search-input" />
        </div>
        <div class="modal-actions">
          <button class="btn btn-small" type="button" @click="uploadFor = null">{{ t('common.cancel') }}</button>
          <button
            class="btn btn-small btn-primary"
            type="button"
            :disabled="uploading || !uploadFile || !uploadName.trim()"
            @click="uploadAsset"
          >{{ t('nexhub.releases.assetUploadBtn') }}</button>
        </div>
      </div>
    </div>

    <!-- 删除附件确认 -->
    <NexhubConfirm
      :open="deletePending !== null"
      :title="deletePending ? t('nexhub.releases.assetDeleteTitle', { name: deletePending.asset.name }) : ''"
      :body="t('nexhub.releases.assetDeleteBody')"
      :danger="true"
      :confirm-text="t('nexhub.releases.assetDeleteAction')"
      @confirm="doDeleteAsset"
      @update:open="(v: boolean) => { if (!v) deletePending = null; }"
    />
  </section>
</template>

<style scoped>
.releases-tab { display: flex; flex-direction: column; gap: 12px; }
.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}
.empty-card { padding: 28px; text-align: center; color: var(--text-muted, #5E5C5F); font-size: 14px; line-height: 1.6; }
.muted { color: var(--text-muted, #5E5C5F); }
.small { font-size: 12px; }
.hint { margin: 0; line-height: 1.6; }
.browser-toolbar { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
.search-input {
  padding: 7px 12px; border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 14px;
  background: var(--bg-card, #fff); color: var(--text, #2B2B2B);
}
.search-input:focus { outline: 2px solid rgba(233, 84, 32, 0.3); border-color: var(--accent, #E95420); }
.notes-input { resize: vertical; }
.release-list { display: flex; flex-direction: column; gap: 10px; }
.release-card { padding: 12px 14px; }
.release-head { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.release-tag {
  display: inline-block; padding: 2px 10px; border-radius: var(--radius-pill, 20px);
  font-size: 12px; font-weight: 700;
  background: rgba(63, 127, 191, 0.14); color: #3573b9;
}
.release-title { font-size: 14.5px; }
.release-notes { margin: 8px 0 0; white-space: pre-wrap; font-size: 13px; line-height: 1.55; }
.head-spacer { flex: 1; }
.asset-list { margin-top: 10px; border-top: 1px dashed rgba(0, 0, 0, 0.12); padding-top: 8px; display: flex; flex-direction: column; gap: 6px; }
.asset-row { display: flex; align-items: center; gap: 10px; flex-wrap: wrap; }
.asset-name { font-size: 13px; font-weight: 600; word-break: break-all; }
.asset-sha {
  font-size: 11px; padding: 1px 8px; border-radius: 6px;
  background: rgba(0, 0, 0, 0.05);
}
.asset-empty { padding: 4px 0; }
.form-row { display: flex; flex-direction: column; gap: 6px; }
.form-label { font-size: 12px; font-weight: 600; color: var(--text-muted, #5E5C5F); }
.form-hint { font-size: 11px; }
.modal-overlay {
  position: fixed; inset: 0; background: rgba(0, 0, 0, 0.4); display: flex;
  align-items: center; justify-content: center; z-index: 1000; padding: 20px;
}
.modal-card { width: 100%; max-width: 460px; padding: 18px 20px; display: flex; flex-direction: column; gap: 12px; }
.modal-head { display: flex; align-items: center; justify-content: space-between; gap: 8px; }
.modal-title { font-size: 16px; font-weight: 700; color: var(--text, #2B2B2B); margin: 0; }
.modal-actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 4px; }
.btn {
  display: inline-flex; align-items: center; gap: 6px; padding: 7px 14px;
  background: var(--bg-card, #fff); border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px); font-size: 13px; font-weight: 500;
  color: var(--text, #2B2B2B); cursor: pointer; font-family: inherit; text-decoration: none;
}
.btn:hover { background: var(--border-soft, #F3F4F6); }
.btn:disabled { opacity: 0.5; cursor: not-allowed; }
.btn-small { padding: 5px 10px; font-size: 12px; }
.btn-primary { background: var(--accent, #E95420); border-color: var(--accent, #E95420); color: #fff; }
.btn-ghost { background: transparent; border-color: transparent; color: var(--accent, #E95420); }
.btn-danger { color: #b91c1c; border-color: rgba(185, 28, 28, 0.3); }
.btn-danger:hover { background: #fee2e2; }
.spin { display: inline-block; }
.spin.spinning { animation: rel-spin 1s linear infinite; }
@keyframes rel-spin { to { transform: rotate(360deg); } }
</style>
