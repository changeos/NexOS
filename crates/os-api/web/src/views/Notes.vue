<script setup lang="ts">
// =============================================================================
// Notes.vue —— 笔记/文档
//
// 布局：左右分栏
//   - 左侧：笔记列表（标题+标签+更新时间，可点选）
//   - 右侧：选中笔记详情（标题可编辑 + content markdown 文本域 + 标签输入 + 保存）
//   - 顶部：新建按钮 + 删除
//
// 后端：GET /api/v1/notes（摘要）/ GET /api/v1/notes/:id（含 content）
//       POST /api/v1/notes / PUT /api/v1/notes/:id / DELETE /api/v1/notes/:id / GET stats
// =============================================================================
import { computed, onMounted, ref } from 'vue';
import { useI18n } from 'vue-i18n';
import { endpoints } from '@/api/client';

const { t } = useI18n();

interface NoteSummary {
  id?: string;
  title?: string;
  tags?: string[];
  updated_at?: string;
  [k: string]: unknown;
}
interface NoteDetail extends NoteSummary {
  content?: string;
  created_at?: string;
}

// =============================================================================
// 列表 + 选中
// =============================================================================
const notes = ref<NoteSummary[]>([]);
const selectedId = ref<string>('');
const detail = ref<NoteDetail | null>(null);
const editTitle = ref('');
const editContent = ref('');
const editTags = ref('');
const stats = ref<{ total_notes: number; total_tags: number; recent_updated: number }>({
  total_notes: 0, total_tags: 0, recent_updated: 0,
});
const loading = ref(false);
const loadingDetail = ref(false);
const saving = ref(false);
const error = ref('');
const msg = ref<{ kind: 'err' | 'ok' | 'info'; text: string } | null>(null);

async function loadList(): Promise<void> {
  loading.value = true;
  error.value = '';
  try {
    const raw = await endpoints.notesList();
    notes.value = Array.isArray(raw) ? (raw as NoteSummary[]) : [];
    // 若当前无选中或选中已被删，默认选第一条
    if (!selectedId.value && notes.value.length) {
      await selectNote(String(notes.value[0].id ?? ''));
    } else if (selectedId.value && !notes.value.some((n) => n.id === selectedId.value)) {
      await selectNote(notes.value.length ? String(notes.value[0].id ?? '') : '');
    }
  } catch (e) {
    notes.value = [];
    error.value = friendlyError(e);
  } finally {
    loading.value = false;
  }
}

async function loadStats(): Promise<void> {
  try {
    const raw = await endpoints.notesStats();
    stats.value = (raw ?? stats.value) as typeof stats.value;
  } catch {
    /* 统计非关键 */
  }
}

async function selectNote(id: string): Promise<void> {
  if (!id) {
    selectedId.value = '';
    detail.value = null;
    editTitle.value = '';
    editContent.value = '';
    editTags.value = '';
    return;
  }
  selectedId.value = id;
  loadingDetail.value = true;
  msg.value = null;
  try {
    const raw = (await endpoints.getNote(id)) as NoteDetail | null;
    detail.value = raw;
    editTitle.value = raw?.title ?? '';
    editContent.value = raw?.content ?? '';
    editTags.value = (raw?.tags ?? []).join(', ');
  } catch (e) {
    detail.value = null;
    msg.value = { kind: 'err', text: t('notes.loadDetailFail', { msg: friendlyError(e) }) };
  } finally {
    loadingDetail.value = false;
  }
}

async function refreshAll(): Promise<void> {
  await Promise.all([loadList(), loadStats()]);
}

// =============================================================================
// 新建 / 保存 / 删除
// =============================================================================
async function createNote(): Promise<void> {
  msg.value = null;
  saving.value = true;
  try {
    const raw = (await endpoints.createNote({ title: t('notes.untitledDefault'), content: '', tags: [] })) as NoteDetail | null;
    const id = String(raw?.id ?? '');
    await refreshAll();
    if (id) await selectNote(id);
    msg.value = { kind: 'ok', text: t('notes.created') };
  } catch (e) {
    msg.value = { kind: 'err', text: t('notes.createFail', { msg: friendlyError(e) }) };
  } finally {
    saving.value = false;
  }
}

async function saveNote(): Promise<void> {
  if (!selectedId.value) return;
  const title = editTitle.value.trim() || t('notes.untitledDefault');
  // 注意：此处形参名 tg（不可用 t——与 useI18n 的 t 遮蔽，v0.1.54 批教训）
  const tags = editTags.value
    .split(',')
    .map((tg) => tg.trim())
    .filter((tg) => tg.length > 0);
  saving.value = true;
  msg.value = { kind: 'info', text: t('notes.saving') };
  try {
    await endpoints.updateNote(selectedId.value, { title, content: editContent.value, tags });
    await refreshAll();
    await selectNote(selectedId.value);
    msg.value = { kind: 'ok', text: t('notes.saved') };
  } catch (e) {
    msg.value = { kind: 'err', text: t('notes.saveFail', { msg: friendlyError(e) }) };
  } finally {
    saving.value = false;
  }
}

async function deleteNote(): Promise<void> {
  if (!selectedId.value) return;
  if (!window.confirm(t('notes.confirmDelete'))) return;
  saving.value = true;
  msg.value = null;
  try {
    await endpoints.deleteNote(selectedId.value);
    selectedId.value = '';
    detail.value = null;
    await refreshAll();
    msg.value = { kind: 'ok', text: t('notes.deleted') };
  } catch (e) {
    msg.value = { kind: 'err', text: t('notes.deleteFail', { msg: friendlyError(e) }) };
  } finally {
    saving.value = false;
  }
}

// =============================================================================
// 工具
// =============================================================================
function formatTime(s?: string): string {
  if (!s) return '—';
  // 取 YYYY-MM-DD HH:MM
  return s.replace('T', ' ').slice(0, 16);
}
function friendlyError(e: unknown): string {
  const m = e instanceof Error ? e.message : String(e);
  if (/404|405|not found|method not allowed/i.test(m)) {
    return t('notes.notImplemented');
  }
  return m;
}

const hasSelection = computed(() => !!selectedId.value && !!detail.value);

onMounted(() => {
  void refreshAll();
});
</script>

<template>
  <div class="notes-page">
    <div class="page-head">
      <div>
        <h2 class="page-title">{{ t('notes.title') }}</h2>
        <div class="page-sub muted">{{ t('notes.subtitle', { notes: stats.total_notes, tags: stats.total_tags }) }}</div>
      </div>
      <div class="head-actions">
        <button class="btn btn-small" :disabled="loading" @click="refreshAll">
          <span class="spin" :class="{ spinning: loading }" aria-hidden="true">↻</span>
          {{ t('notes.refresh') }}
        </button>
        <button class="btn btn-small btn-primary" :disabled="saving" @click="createNote">{{ t('notes.create') }}</button>
      </div>
    </div>

    <div v-if="error" class="error-box">{{ error }}</div>

    <section class="notes-split">
      <!-- 左侧：列表 -->
      <div class="card list-pane">
        <div v-if="loading && !notes.length" class="pane-empty">{{ t('notes.loading') }}</div>
        <div v-else-if="!notes.length" class="pane-empty">{{ t('notes.emptyList') }}</div>
        <ul v-else class="note-list">
          <li
            v-for="n in notes"
            :key="n.id"
            :class="['note-item', { active: n.id === selectedId }]"
            @click="selectNote(String(n.id ?? ''))"
          >
            <div class="note-item-title">{{ n.title ?? t('notes.untitled') }}</div>
            <div class="note-item-meta">
              <span class="note-item-time">{{ formatTime(n.updated_at) }}</span>
              <!-- v-for 形参不可命名 t（与 useI18n 的 t 遮蔽，v0.1.56 批教训） -->
              <span v-for="tg in n.tags" :key="tg" class="tag">{{ tg }}</span>
            </div>
          </li>
        </ul>
      </div>

      <!-- 右侧：详情 -->
      <div class="card detail-pane">
        <div v-if="loadingDetail" class="pane-empty">{{ t('notes.loading') }}</div>
        <div v-else-if="!hasSelection" class="pane-empty">{{ t('notes.selectHint') }}</div>
        <div v-else class="detail-body">
          <div class="detail-toolbar">
            <input v-model="editTitle" class="title-input" :placeholder="t('notes.titlePh')" :disabled="saving" />
            <button class="btn btn-small btn-danger" :disabled="saving" @click="deleteNote">{{ t('notes.delete') }}</button>
          </div>
          <div class="field-tags">
            <label>{{ t('notes.tagsLabel') }}</label>
            <input v-model="editTags" type="text" :placeholder="t('notes.tagsPh')" :disabled="saving" />
          </div>
          <div class="editor-area">
            <textarea
              v-model="editContent"
              class="content-editor"
              :placeholder="t('notes.contentPh')"
              :disabled="saving"
              spellcheck="false"
            ></textarea>
            <div class="content-preview">
              <div class="preview-label">{{ t('notes.preview') }}</div>
              <pre class="preview-body">{{ editContent || t('notes.emptyContent') }}</pre>
            </div>
          </div>
          <div class="detail-foot">
            <span class="muted">{{ t('notes.updatedAt', { time: formatTime(detail?.updated_at) }) }}</span>
            <div class="foot-actions">
              <p v-if="msg" :class="['form-msg', `is-${msg.kind}`]">{{ msg.text }}</p>
              <button class="btn btn-primary" :disabled="saving" @click="saveNote">
                {{ saving ? t('notes.saving') : t('notes.save') }}
              </button>
            </div>
          </div>
        </div>
      </div>
    </section>
  </div>
</template>

<style scoped>
.notes-page {
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

.card {
  background: var(--bg-card, #fff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}

.notes-split { display: grid; grid-template-columns: 300px 1fr; gap: 14px; min-height: 60vh; }
@media (max-width: 820px) { .notes-split { grid-template-columns: 1fr; } }

.list-pane { padding: 8px; overflow: auto; max-height: 70vh; }
.pane-empty { padding: 32px 16px; text-align: center; color: var(--text-muted, #5E5C5F); font-size: 14px; }
.note-list { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: 2px; }
.note-item { padding: 10px 12px; border-radius: var(--radius-sm, 8px); cursor: pointer; transition: background 0.12s ease; }
.note-item:hover { background: rgba(0, 0, 0, 0.04); }
.note-item.active { background: rgba(233, 84, 32, 0.10); }
.note-item-title { font-size: 14px; font-weight: 600; color: var(--text, #2B2B2B); }
.note-item-meta { margin-top: 4px; display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
.note-item-time { font-size: 11px; color: var(--text-muted, #5E5C5F); }
.tag { font-size: 11px; padding: 1px 7px; border-radius: var(--radius-pill, 20px); background: rgba(233, 84, 32, 0.10); color: #C7421A; }

.detail-pane { padding: 16px 18px; display: flex; flex-direction: column; gap: 12px; min-height: 60vh; }
.detail-body { display: flex; flex-direction: column; gap: 12px; flex: 1; }
.detail-toolbar { display: flex; gap: 10px; align-items: center; }
.title-input { flex: 1; font-size: 18px; font-weight: 700; color: var(--text, #2B2B2B); border: none; border-bottom: 1px solid var(--border-soft, #EDEDED); padding: 6px 2px; background: transparent; font-family: inherit; }
.title-input:focus { outline: none; border-bottom-color: var(--accent, #E95420); }
.field-tags { display: flex; flex-direction: column; gap: 4px; }
.field-tags label { font-size: 12px; color: var(--text-muted, #5E5C5F); font-weight: 500; }
.field-tags input { width: 100%; padding: 6px 10px; border: 1px solid var(--border, #d1d5db); border-radius: var(--radius-sm, 8px); font-family: inherit; font-size: 13px; }

.editor-area { display: grid; grid-template-columns: 1fr 1fr; gap: 12px; flex: 1; min-height: 320px; }
@media (max-width: 820px) { .editor-area { grid-template-columns: 1fr; } }
.content-editor { width: 100%; min-height: 320px; resize: vertical; padding: 12px; border: 1px solid var(--border, #d1d5db); border-radius: var(--radius-sm, 8px); font-family: var(--mono); font-size: 13px; line-height: 1.5; color: var(--text, #2B2B2B); background: var(--bg-card, #fff); }
.content-preview { display: flex; flex-direction: column; border: 1px solid var(--border, #d1d5db); border-radius: var(--radius-sm, 8px); overflow: hidden; min-height: 320px; }
.preview-label { font-size: 12px; color: var(--text-muted, #5E5C5F); padding: 6px 12px; background: var(--border-soft, #FAFAFA); border-bottom: 1px solid var(--border-soft, #EDEDED); font-weight: 600; }
.preview-body { flex: 1; margin: 0; padding: 12px; font-family: var(--mono); font-size: 13px; line-height: 1.5; white-space: pre-wrap; word-break: break-word; overflow: auto; color: var(--text, #2B2B2B); }

.detail-foot { display: flex; justify-content: space-between; align-items: center; gap: 12px; padding-top: 6px; border-top: 1px solid var(--border-soft, #EDEDED); }
.foot-actions { display: flex; align-items: center; gap: 12px; }

.form-msg { font-size: 13px; padding: 2px 0; }
.form-msg.is-err { color: #b91c1c; }
.form-msg.is-ok { color: #15803d; }
.form-msg.is-info { color: var(--text-muted, #6b7280); }
.error-box { color: #b91c1c; background: #fee2e2; border: 1px solid rgba(185, 28, 28, 0.2); padding: 10px 14px; border-radius: var(--radius-sm, 8px); font-size: 13px; }

.btn {
  padding: 6px 14px; border-radius: var(--radius-sm, 8px);
  border: 1px solid var(--border, #d1d5db); background: var(--bg-card, #fff);
  color: var(--text, #2B2B2B); font-size: 13px; cursor: pointer; font-family: inherit;
  transition: background 0.15s ease;
}
.btn:hover { background: rgba(0, 0, 0, 0.04); }
.btn:disabled { opacity: 0.5; cursor: not-allowed; }
.btn-small { padding: 4px 10px; font-size: 12.5px; }
.btn-primary { background: var(--accent, #E95420); color: #fff; border-color: var(--accent, #E95420); }
.btn-primary:hover:not(:disabled) { background: var(--accent-hi, #0077ed); }
.btn-danger { color: #b91c1c; border-color: rgba(185, 28, 28, 0.35); background: #fff5f5; }
.btn-danger:hover:not(:disabled) { background: #fee2e2; }

.spin { display: inline-block; font-size: 14px; line-height: 1; }
.spin.spinning { animation: spin 0.8s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }
</style>
