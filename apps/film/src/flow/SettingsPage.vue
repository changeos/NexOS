<script setup lang="ts">
// =============================================================================
// SettingsPage.vue —— 设置/成员页（多人分工 v1 分区认领，FilmHub v0.1.35）。
//
// ① 项目成员管理（添加成员名/移除——ownership.json members）；
// ② 分区认领（story/storyboard/casting/audio/compose 各分区指定负责人 owner
//    或释放；PUT files/ownership.json 走通用 files 面）；
// ③ 项目活动流（activity.json 最近条目 [{ts,author,action,target}]，新→旧；
//    refreshTick 联动刷新）。
// 页头如实说明：多人同时编辑以后保存为准（v1 无锁，并发协作等 P1 git 仓化）。
// 「我是」选择器在工作室顶栏（FilmStudio）——本页亦可改（同一 localStorage）。
// =============================================================================
import { computed, onMounted, ref, watch } from 'vue'
import { useI18n } from 'vue-i18n'
import { ANONYMOUS_AUTHOR, claimSection, fmtActivityTime, normalizeMembers } from './collab'
import { useFlow } from './flowContext'
import { FLOW_STAGES, stageIndex, type FilmStage } from './flowTypes'

const { t } = useI18n()
const ctx = useFlow()

// —— 阶段标签（分区认领行）——
const STAGE_LABEL_KEYS: Record<FilmStage, string> = {
  story: 'film.flowStory',
  storyboard: 'film.flowStoryboard',
  casting: 'film.flowCasting',
  audio: 'film.flowAudio',
  compose: 'film.flowCompose',
}
function stageLabel(s: FilmStage): string {
  return t(STAGE_LABEL_KEYS[s])
}

const members = computed<string[]>(() => ctx?.ownership.value?.members ?? [])
const sectionOwners = computed<Record<string, string>>(() => {
  const out: Record<string, string> = {}
  const sections = ctx?.ownership.value?.sections ?? {}
  for (const [k, v] of Object.entries(sections)) {
    out[k] = typeof v === 'string' ? v : (v?.owner ?? '')
  }
  return out
})

// —— 成员管理 ——
const memberInput = ref('')
const memberError = ref('')
const saveError = ref('')
const saving = ref(false)

async function addMember(): Promise<void> {
  if (!ctx || saving.value) return
  const name = memberInput.value.trim()
  if (!name) {
    memberError.value = t('film.ownErrMember')
    return
  }
  if (members.value.includes(name)) {
    memberError.value = t('film.ownErrMemberDup')
    return
  }
  memberError.value = ''
  saving.value = true
  try {
    const next = {
      ...(ctx.ownership.value ?? {}),
      members: normalizeMembers([...members.value, name]),
    }
    if (await ctx.saveOwnership(next)) memberInput.value = ''
    else saveError.value = t('film.ownSaveFailed')
  } finally {
    saving.value = false
  }
}

async function removeMember(name: string): Promise<void> {
  if (!ctx || saving.value) return
  if (!window.confirm(t('film.ownRemoveConfirm', { name }))) return
  saving.value = true
  try {
    if (
      !(await ctx.saveOwnership({
        ...(ctx.ownership.value ?? {}),
        members: normalizeMembers(members.value.filter((m) => m !== name)),
      }))
    ) {
      saveError.value = t('film.ownSaveFailed')
    }
  } finally {
    saving.value = false
  }
}

// —— 分区认领 ——
async function setSectionOwner(stage: FilmStage, owner: string): Promise<void> {
  if (!ctx || saving.value) return
  saving.value = true
  try {
    if (!(await ctx.saveOwnership(claimSection(ctx.ownership.value, stage, owner)))) {
      saveError.value = t('film.ownSaveFailed')
    }
  } finally {
    saving.value = false
  }
}

// —— 活动流 ——
const activity = computed(() => ctx?.activity.value ?? [])

onMounted(() => void ctx?.refreshCollab())
watch(
  () => ctx?.refreshTick.value,
  () => void ctx?.refreshCollab(),
)

// —— 「我是」快捷改（与顶栏同一 context.author/localStorage）——
const authorDraft = ref('')
async function applyAuthor(): Promise<void> {
  if (!ctx) return
  ctx.setAuthor(authorDraft.value)
  authorDraft.value = ''
}
const currentAuthor = computed(() => ctx?.author.value ?? ANONYMOUS_AUTHOR)
</script>

<template>
  <div class="fh-page">
    <!-- 定妆/成员页头不挂分区 owner（本页即管理面）——直接用通用 head 样式 -->
    <div class="fh-head">
      <span class="fh-head-title">{{ t('film.flowSettings') }}</span>
      <div class="fh-head-actions">
        <span class="fh-pill fh-pill-muted">👤 {{ t('film.whoAmI') }}：{{ currentAuthor }}</span>
      </div>
    </div>
    <div class="fh-collab-tip">{{ t('film.collabTip') }}</div>
    <div v-if="saveError" class="fh-error-box">{{ saveError }}</div>

    <div class="fh-page-scroll fh-two-col">
      <!-- 左：成员管理 + 分区认领 -->
      <section class="fh-card fh-col">
        <div class="fh-card-head">
          <span>👥 {{ t('film.ownMembers') }}</span>
          <span class="fh-muted fh-small">{{ members.length }}</span>
        </div>
        <div class="fh-card-body">
          <!-- 「我是」改写 -->
          <div class="fh-field-row">
            <input
              v-model="authorDraft"
              type="text"
              class="fh-input"
              style="flex: 1"
              :placeholder="t('film.whoAmIPh')"
            >
            <button class="fh-btn fh-btn-small" type="button" :disabled="!ctx" @click="applyAuthor">
              {{ t('film.whoAmIApply') }}
            </button>
          </div>
          <div class="fh-field-row">
            <input
              v-model="memberInput"
              type="text"
              class="fh-input"
              style="flex: 1"
              :placeholder="t('film.ownMemberPh')"
              @keydown.enter="addMember"
            >
            <button class="fh-btn fh-btn-primary fh-btn-small" type="button" :disabled="saving" @click="addMember">
              + {{ t('film.ownAddMember') }}
            </button>
          </div>
          <div v-if="memberError" class="fh-error-box">{{ memberError }}</div>
          <div v-if="!members.length" class="fh-empty">{{ t('film.ownMembersEmpty') }}</div>
          <div v-for="m in members" :key="m" class="fh-row">
            <span class="fh-pill fh-pill-blue">👤 {{ m }}</span>
            <span v-if="m === currentAuthor" class="fh-pill fh-pill-ok fh-pill-mini">{{ t('film.whoAmI') }}</span>
            <button
              class="fh-btn fh-btn-mini fh-btn-danger"
              style="margin-left: auto"
              type="button"
              :disabled="saving"
              @click="removeMember(m)"
            >{{ t('film.ownRemoveMember') }}</button>
          </div>

          <!-- 分区认领 -->
          <div class="fh-field-label" style="margin-top: 6px">{{ t('film.ownTitle') }}</div>
          <div v-for="s in FLOW_STAGES" :key="s" class="fh-row">
            <span class="fh-mono fh-muted">{{ ['①', '②', '③', '④', '⑤'][stageIndex(s)] }}</span>
            <span style="min-width: 64px">{{ stageLabel(s) }}</span>
            <span
              v-if="sectionOwners[s]"
              class="fh-owner"
            >👤 {{ sectionOwners[s] }}</span>
            <span v-else class="fh-owner is-unclaimed">{{ t('film.ownUnclaimed') }}</span>
            <select
              class="fh-select"
              style="margin-left: auto; min-width: 130px"
              :value="sectionOwners[s] || ''"
              :disabled="saving"
              @change="setSectionOwner(s, ($event.target as HTMLSelectElement).value)"
            >
              <option value="">— {{ t('film.ownUnclaimed2') }} —</option>
              <option v-for="m in members" :key="m" :value="m">{{ m }}</option>
            </select>
          </div>
        </div>
      </section>

      <!-- 右：活动流 -->
      <section class="fh-card fh-col">
        <div class="fh-card-head">
          <span>📜 {{ t('film.actTitle') }}</span>
          <span class="fh-muted fh-small">{{ activity.length }}</span>
          <div class="fh-head-actions">
            <button class="fh-btn fh-btn-small" type="button" @click="ctx?.refreshCollab()">↻</button>
          </div>
        </div>
        <div class="fh-card-body">
          <div v-if="!activity.length" class="fh-empty">{{ t('film.actEmpty') }}</div>
          <div v-for="(a, i) in activity" :key="i" class="fh-row" style="flex-wrap: nowrap">
            <span class="fh-muted fh-small fh-mono" style="flex-shrink: 0">
              {{ fmtActivityTime(a.ts) }}
            </span>
            <span class="fh-pill fh-pill-mini fh-pill-blue" style="flex-shrink: 0">{{ a.author || ANONYMOUS_AUTHOR }}</span>
            <span class="fh-small" style="flex-shrink: 0">{{ a.action || '—' }}</span>
            <span class="fh-mono fh-small fh-ellipsis" style="flex: 1; min-width: 0" :title="a.target ?? ''">
              {{ a.target || '' }}
            </span>
          </div>
          <div class="fh-muted fh-small">{{ t('film.actHint') }}</div>
        </div>
      </section>
    </div>
  </div>
</template>
