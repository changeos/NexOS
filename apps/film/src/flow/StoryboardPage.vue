<script setup lang="ts">
// =============================================================================
// StoryboardPage.vue —— 分镜页（流程 ②，FilmHub v0.1.35）。
//
// 顶部「从剧情生成分镜」（chat model_ref + 已有分镜先确认覆盖 → storyboard/
// generate 任务轮询；v0.1.38 选择器首项「项目默认」= 不传 model_ref，走
// models.json chat 缺省链）+ 生成分镜配置面板（v0.1.39 共用
// StoryboardGenPanel：出场人物/声线/镜头数/时长提示——配置随请求 body）；
// 生成后镜头卡横排网格预览（号 / desc / 时长 / casting 空槽标记 + 产物状态
// 点）+「去工作台细调」按钮（切 workbench 视图，五区编辑/生成/预览）。
// 分镜数据来自项目详情 script（任务终态 reloadProject 联动）。
// =============================================================================
import { computed, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { filmStoryboardGenerate, type FilmShot } from '../api'
import FlowPageHead from './FlowPageHead.vue'
import NxButton from '../nx/NxButton.vue'
import NxThemeToggle from '../nx/NxThemeToggle.vue'
import StoryboardGenPanel from './StoryboardGenPanel.vue'
import { PROJECT_DEFAULT_KEY, useFlow } from './flowContext'
// v0.1.11 全局操作反馈：提交失败全局 toast（成功/终态走任务通知闭环）
import { toast } from '../nx/toast'

const { t } = useI18n()
const ctx = useFlow()

const genBusy = ref(false)
const genError = ref('')
/** 生成分镜配置面板（本页「从剧情生成分镜」共用；config() 取拼装结果）。 */
const genPanel = ref<InstanceType<typeof StoryboardGenPanel> | null>(null)

const shots = computed<FilmShot[]>(() => ctx?.project.value?.script ?? [])

/** 产物是否存在（项目详情 artifacts 清单；文件名口径与工作台一致）。 */
function hasArtifact(name: string): boolean {
  return (ctx?.project.value?.artifacts ?? []).some((a) => a.name === name)
}

function shotState(s: FilmShot): { icon: string; label: string } {
  if (hasArtifact(`shot-${s.shot}.mp4`)) return { icon: '▶', label: t('film.stVideo') }
  if (hasArtifact(`line-${s.shot}.mp3`)) return { icon: '🔊', label: t('film.stTts') }
  if (hasArtifact(`shot-${s.shot}.png`)) return { icon: '🖼', label: t('film.stImage') }
  return { icon: '📝', label: t('film.stPending') }
}

/** 从剧情生成分镜（已有分镜先确认覆盖；「项目默认」= 不传 model_ref；
 *  v0.1.39 配置面板字段随 body 发出——后端 v1 未消费，面板内已标注）。 */
async function generateStoryboard(): Promise<void> {
  if (!ctx || genBusy.value) return
  if (shots.value.length && !window.confirm(t('film.sbOverwriteHint'))) return
  const useDefault = ctx.isProjectDefaultSel('chat')
  const ref = ctx.modelRefFor('chat')
  if (!useDefault && !ref) {
    genError.value = ctx.hasOptionsFor('chat') ? t('film.pickModel') : t('film.noSource')
    return
  }
  genBusy.value = true
  genError.value = ''
  try {
    const task = await filmStoryboardGenerate(
      ctx.project.value!.id,
      ref ?? undefined,
      ctx.author.value,
      genPanel.value?.config(),
    )
    // 任务中心登记（v0.1.6：stage 显式传 storyboard，与管线按钮同路）
    ctx.trackFilmTask(task.id, 'storyboard')
  } catch (e) {
    const msg = ctx.errMsg(e)
    genError.value = t('film.actFailed') + msg
    // v0.1.11：提交失败全局可见
    toast.error(t('film.actFailed') + msg)
  } finally {
    genBusy.value = false
  }
}

/** 去工作台细调（切 workbench 视图——五区：镜头面板/监视器/时间轴）。 */
function toWorkbench(): void {
  ctx?.setView('workbench')
}
</script>

<template>
  <div class="fh-page nx-page">
    <FlowPageHead stage="storyboard" :title="t('film.flowStoryboard')">
      <template #actions>
        <NxThemeToggle />
        <span class="fh-pill fh-pill-muted">{{ t('film.sbShots', { n: shots.length }) }}</span>
        <select
          v-if="ctx"
          v-model="ctx.modelSel.chat"
          class="fh-select"
          :title="t('film.model')"
        >
          <option :value="PROJECT_DEFAULT_KEY">
            🏷 {{ t('models.projectDefault') }}{{ ctx.defaultModelSummary('chat') ? ' · ' + ctx.defaultModelSummary('chat') : '' }}
          </option>
          <option v-if="!ctx.hasOptionsFor('chat')" value="" disabled>
            {{ t('film.noRunningLlm') }}
          </option>
          <optgroup v-for="g in ctx.optionsFor('chat')" :key="g.label" :label="g.label">
            <option v-for="o in g.options" :key="o.key" :value="o.key">
              {{ o.label }}{{ o.relay ? ' 🌐' : '' }}
            </option>
          </optgroup>
        </select>
        <NxButton
          variant="primary"
          size="sm"
          :loading="genBusy"
          :disabled="!ctx?.project.value || !ctx?.modelSelReady('chat') || !ctx?.chatAvailable.value || ctx?.isOffline.value"
          @click="generateStoryboard"
        >{{ genBusy ? t('film.btnBusy') : t('film.sbGenerate') }}</NxButton>
        <button
          v-if="shots.length"
          class="fh-btn fh-btn-small"
          type="button"
          :title="t('film.sbWorkbenchTip')"
          @click="toWorkbench"
        >🛠 {{ t('film.sbToWorkbench') }}</button>
      </template>
    </FlowPageHead>

    <!-- 生成分镜配置面板（v0.1.39：出场人物 chips + 声线下拉 + 镜头数 +
         总时长提示；折叠态默认收起，配置随「从剧情生成分镜」请求发出） -->
    <StoryboardGenPanel ref="genPanel" />

    <div v-if="genError" class="fh-error-box">{{ genError }}</div>

    <div class="fh-page-scroll">
      <div v-if="!shots.length" class="fh-empty">
        {{ t('film.sbEmpty') }}
        <div class="fh-muted fh-small sb-need-hint">{{ t('film.sbNeedStory') }}</div>
      </div>
      <!-- 镜头卡横排网格预览（nx-card 静默刻面 + hover 浮起；点击=去工作台细调） -->
      <div v-else class="fh-grid">
        <div
          v-for="s in shots"
          :key="s.shot"
          class="fh-card sb-card"
          role="button"
          tabindex="0"
          @click="toWorkbench"
          @keydown.enter="toWorkbench"
        >
          <div class="sb-card-top">
            <span class="fh-mono sb-card-no">#{{ s.shot }}</span>
            <span class="sb-card-state">{{ shotState(s).icon }} {{ shotState(s).label }}</span>
            <span
              v-if="typeof s.duration_secs === 'number' && s.duration_secs > 0"
              class="fh-pill fh-pill-muted fh-pill-mini sb-card-dur"
            >{{ t('film.sbDuration', { n: s.duration_secs }) }}</span>
          </div>
          <div class="sb-card-desc">
            {{ s.desc || '—' }}
          </div>
          <!-- casting 槽标记：绑定角色 chips / 空槽虚线「待定妆」 -->
          <div class="sb-card-cast">
            <template v-if="(s.characters ?? []).length">
              <span v-for="name in s.characters" :key="name" class="fh-pill fh-pill-blue fh-pill-mini">
                👤 {{ name }}
              </span>
            </template>
            <span
              v-else
              class="fh-pill fh-pill-muted fh-pill-mini sb-cast-empty"
              :title="t('film.sbCastEmptyTip')"
            >{{ t('film.sbCastEmpty') }}</span>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>

<style scoped>
/* v0.1.8 nx 化：镜头网格卡（布局类；卡面/描边/阴影刻面在 .nx-page .fh-card） */
.sb-card {
  padding: 10px 12px;
  gap: 6px;
  cursor: pointer;
  transition:
    border-color 0.15s ease,
    box-shadow 0.15s ease,
    background 0.15s ease;
}
.sb-card:hover {
  border-color: var(--nx-line-strong);
  box-shadow: var(--nx-shadow-md);
  background: var(--nx-bg-subtle);
}
.sb-card:focus-visible {
  outline: 2px solid var(--nx-ring-color);
  outline-offset: 1px;
}
.sb-card-top {
  display: flex;
  align-items: center;
  gap: 8px;
}
.sb-card-no {
  font-weight: var(--nx-font-weight-semibold);
  color: var(--nx-text-primary);
}
.sb-card-state {
  font-size: var(--nx-font-size-xs);
  color: var(--nx-text-tertiary);
}
.sb-card-dur { margin-left: auto; }
.sb-card-desc {
  font-size: var(--nx-font-size-xs);
  color: var(--nx-text-secondary);
  line-height: 1.5;
  min-height: 2.9em;
  overflow: hidden;
}
.sb-card-cast {
  display: flex;
  gap: 5px;
  flex-wrap: wrap;
  align-items: center;
}
.sb-cast-empty { border-style: dashed; }
.sb-need-hint { margin-top: 6px; }
</style>
