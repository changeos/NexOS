<script setup lang="ts">
// =============================================================================
// FlowPageHead.vue —— 流程页通用页头（FilmHub v0.1.35）。
//
// 标题 + 阶段序徽章 + 分区负责人（ownership.sections；未认领=「待认领」虚线
// 徽章 + 一键认领按钮）+ 右侧操作区（slot actions）。成员 ≥2 时页头下出现
// 「多人同时编辑以后保存为准」提示条（v1 无锁软约束，P1 git 仓化解决）。
// =============================================================================
import { computed } from 'vue'
import { useI18n } from 'vue-i18n'
import { claimSection, sectionOwner } from './collab'
import { useFlow } from './flowContext'
import { stageBadge, stageIndex, type FilmStage } from './flowTypes'

const props = defineProps<{
  /** 分区（负责人/认领归属）。 */
  stage: FilmStage
  /** 页标题 i18n 后的文案（由调用方 t() 后传入）。 */
  title: string
}>()

const { t } = useI18n()
const ctx = useFlow()

/** 分区负责人（''=未认领）。 */
const owner = computed(() => (ctx ? sectionOwner(ctx.ownership.value, props.stage) : ''))

/** 多人提示（成员 ≥2 才出现——单人项目无噪音）。 */
const multiMember = computed(() => (ctx?.ownership.value?.members?.length ?? 0) >= 2)

/** 一键认领（当前操作人认领本分区）。 */
async function claimMe(): Promise<void> {
  if (!ctx || owner.value) return
  await ctx.saveOwnership(claimSection(ctx.ownership.value, props.stage, ctx.author.value))
}
</script>

<template>
  <div class="fh-head">
    <span class="fh-head-title">{{ title }}</span>
    <span class="fh-pill fh-pill-muted fh-pill-mini">{{ stageBadge(stageIndex(stage)) }}</span>
    <!-- 分区负责人：已认领=蓝徽章；未认领=虚线徽章+「认领」 -->
    <span
      v-if="owner"
      class="fh-owner"
      :title="t('film.ownOwnerTip')"
    >👤 {{ t('film.ownOwner', { name: owner }) }}</span>
    <template v-else>
      <span class="fh-owner is-unclaimed" :title="t('film.ownUnclaimedTip')">
        {{ t('film.ownUnclaimed') }}
      </span>
      <button
        v-if="ctx"
        class="fh-btn fh-btn-mini"
        type="button"
        :title="t('film.ownClaimTip', { name: ctx.author.value })"
        @click="claimMe"
      >{{ t('film.ownClaim') }}</button>
    </template>
    <div class="fh-head-actions"><slot name="actions" /></div>
  </div>
  <!-- 多人协作提示（v1 软约束；并发协作等 P1 git 仓化） -->
  <div v-if="multiMember" class="fh-collab-tip">{{ t('film.collabTip') }}</div>
</template>
