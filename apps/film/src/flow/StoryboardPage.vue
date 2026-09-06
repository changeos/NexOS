<script setup lang="ts">
// =============================================================================
// StoryboardPage.vue —— 分镜页（流程 ②，FilmHub v0.1.35）。
//
// 顶部「从剧情生成分镜」（chat model_ref + 已有分镜先确认覆盖 → storyboard/
// generate 任务轮询）；生成后镜头卡横排网格预览（号 / desc / 时长 / casting
// 空槽标记 + 产物状态点）+「去工作台细调」按钮（切 workbench 视图，五区
// 编辑/生成/预览）。分镜数据来自项目详情 script（任务终态 reloadProject 联动）。
// =============================================================================
import { computed, ref } from 'vue'
import { useI18n } from 'vue-i18n'
import { filmStoryboardGenerate, type FilmShot } from '../api'
import FlowPageHead from './FlowPageHead.vue'
import { useFlow } from './flowContext'

const { t } = useI18n()
const ctx = useFlow()

const genBusy = ref(false)
const genError = ref('')

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

/** 从剧情生成分镜（已有分镜先确认覆盖）。 */
async function generateStoryboard(): Promise<void> {
  if (!ctx || genBusy.value) return
  if (shots.value.length && !window.confirm(t('film.sbOverwriteHint'))) return
  const ref = ctx.modelRefFor('chat')
  if (!ref) {
    genError.value = ctx.hasOptionsFor('chat') ? t('film.pickModel') : t('film.noSource')
    return
  }
  genBusy.value = true
  genError.value = ''
  try {
    const task = await filmStoryboardGenerate(ctx.project.value!.id, ref, ctx.author.value)
    ctx.addTracked(task)
  } catch (e) {
    genError.value = t('film.actFailed') + ctx.errMsg(e)
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
  <div class="fh-page">
    <FlowPageHead stage="storyboard" :title="t('film.flowStoryboard')">
      <template #actions>
        <span class="fh-pill fh-pill-muted">{{ t('film.sbShots', { n: shots.length }) }}</span>
        <select
          v-if="ctx"
          v-model="ctx.modelSel.chat"
          class="fh-select"
          :title="t('film.model')"
        >
          <option v-if="!ctx.hasOptionsFor('chat')" value="" disabled>
            {{ t('film.noRunningLlm') }}
          </option>
          <optgroup v-for="g in ctx.optionsFor('chat')" :key="g.label" :label="g.label">
            <option v-for="o in g.options" :key="o.key" :value="o.key">
              {{ o.label }}{{ o.relay ? ' 🌐' : '' }}
            </option>
          </optgroup>
        </select>
        <button
          class="fh-btn fh-btn-primary fh-btn-small"
          type="button"
          :disabled="genBusy || !ctx?.project.value || !ctx?.hasOptionsFor('chat') || !ctx?.chatAvailable.value || ctx?.isOffline.value"
          @click="generateStoryboard"
        >{{ genBusy ? t('film.taskRunning') + '…' : t('film.sbGenerate') }}</button>
        <button
          v-if="shots.length"
          class="fh-btn fh-btn-small"
          type="button"
          :title="t('film.sbWorkbenchTip')"
          @click="toWorkbench"
        >🛠 {{ t('film.sbToWorkbench') }}</button>
      </template>
    </FlowPageHead>

    <div v-if="genError" class="fh-error-box">{{ genError }}</div>

    <div class="fh-page-scroll">
      <div v-if="!shots.length" class="fh-empty">
        {{ t('film.sbEmpty') }}
        <div class="fh-muted fh-small" style="margin-top: 6px">{{ t('film.sbNeedStory') }}</div>
      </div>
      <!-- 镜头卡横排网格预览 -->
      <div v-else class="fh-grid">
        <div
          v-for="s in shots"
          :key="s.shot"
          class="fh-card"
          style="padding: 10px 12px; gap: 6px; cursor: pointer"
          @click="toWorkbench"
        >
          <div style="display: flex; align-items: center; gap: 8px">
            <span class="fh-mono" style="font-weight: 700">#{{ s.shot }}</span>
            <span class="fh-muted fh-small">{{ shotState(s).icon }} {{ shotState(s).label }}</span>
            <span
              v-if="typeof s.duration_secs === 'number' && s.duration_secs > 0"
              class="fh-pill fh-pill-muted fh-pill-mini"
              style="margin-left: auto"
            >{{ t('film.sbDuration', { n: s.duration_secs }) }}</span>
          </div>
          <div class="fh-small" style="line-height: 1.5; min-height: 2.9em; overflow: hidden">
            {{ s.desc || '—' }}
          </div>
          <!-- casting 槽标记：绑定角色 chips / 空槽虚线「待定妆」 -->
          <div style="display: flex; gap: 5px; flex-wrap: wrap; align-items: center">
            <template v-if="(s.characters ?? []).length">
              <span v-for="name in s.characters" :key="name" class="fh-pill fh-pill-blue fh-pill-mini">
                👤 {{ name }}
              </span>
            </template>
            <span
              v-else
              class="fh-pill fh-pill-muted fh-pill-mini"
              style="border-style: dashed"
              :title="t('film.sbCastEmptyTip')"
            >{{ t('film.sbCastEmpty') }}</span>
          </div>
        </div>
      </div>
    </div>
  </div>
</template>
