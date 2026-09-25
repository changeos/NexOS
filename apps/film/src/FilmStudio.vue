<script setup lang="ts">
// =============================================================================
// FilmStudio.vue —— 影片制作（AI 影片管线，NexOS 应用包 nexos-app-film）
//
// 自主前端（crates/os-api/web）剥离为独立应用包：HTTP 层走宿主桥（./api.ts，
// __NEXOS_HOST__.api），vue / vue-i18n 同样经宿主桥取主前端实例（vite.config.ts
// host-externals 构建期重写）——响应式与 useI18n 与宿主共享。
//
// 参考 LibTV 管线：创意 → 剧本（分镜）→ 分镜图 → 图生视频 → 配音 → BGM → 合成。
// 每个生成阶段可选模型源（model_ref）：
//   - local：本地能力（chat=运行中 LLM 实例；image=本地 sd-turbo）
//   - channel：网关渠道（GET /gateway/channels；via_node 非空 = 🌐 联邦中继渠道）
//     video/tts/music 仅渠道源（提示「需在网关配置视频/配音渠道」）
//
// 两态视图（组件内切换，不走路由）：
//   1. FilmHub 大厅（v0.1.1 显性大厅，HubLobby 组件）：🎬 品牌栏（影片项目
//      中心 · AI 像写代码一样创作）+ 搜索（标题/idea）+ 丰富模式开关（并发
//      ≤12 项目轻读 ownership/activity/cost，失败静默降级素卡）+ 项目卡网格
//      （NexHub 仓库卡风格：五阶段进度点按产物启发式推导）+ 新建对话框
//      （三段式钉底，弹窗留在本组件）；
//   2. 项目工作室（v0.1.35 FilmHub 流程化 + v0.1.1 第八视图「Hub 浏览」）：
//      左侧竖向选项卡栏（SideNav v0.1.5 排序：🎬 FilmHub 回大厅 + 五流程
//      阶段 剧情/分镜/定妆/音频/合成 + 工作台（流程尾部）+ 设置/成员 + 模型
//      设置；「Hub 浏览」导航项已删（与左下树卡重复）——hub 视图保留，经树卡
//      「完整浏览」/树卡点文件/深链 view=hub 到达（HubBrowse：项目文件树 +
//      内容区 + 活动/成本/接入指南 Tab）；阶段徽章读 README stage；每行行尾
//      ⧉ 外链钮 = 新标签页打开该视图独立页；底部树卡可 ⤢ 原位大展开）+
//      顶栏（「我是」操作人 + 成本徽章）+ 页面主体：
//      · 视图深链（v0.1.5）：standalone.html?p=<id>&view=<view>（casting 加
//        &cast=<type>/<name>、hub 加 &file=<path>）——启动解析直达工作室。
//      · 五个流程页各自独立组件（src/flow/*Page.vue；FlowContext 共享会话态）
//      · 「工作台」= 原五区（v0.1.35）：左镜头卡纵列 24% + 底部紧凑任务条 /
//        中镜头面板 40%（角色区在面板列内）/ 右预览监视器 36%（PreviewMonitor）
//        + 底部多轨时间轴条（TimelineTracks，四轨 + 播放头，可折叠）——
//        previewEngine.ts provide/inject 共享（final 模式支持 dist 版本文件）。
//
// v0.1.45 短片生成（ShotGen）为应用**默认首屏**（mode='shotgen'，对标海螺
// H3 创作页的直连 video 渠道单段生成器——不建项目不走分镜/生图/合成管线）；
// 全流程（大厅 + 工作室）降为「高级模式」入口（ShotGen 顶栏「高级模式 →」/
// 作品黄条「去配置」跳模型设置）。深链 view=shotgen 显式直达首屏；带 p 的
// 项目深链仍直达工作室（p 优先）。
//
// 多人分工 v1（分区认领 + 定妆对象级认领）：ownership.json / activity.json 走
// files 面；写操作带 author（「我是」选择器，localStorage 记忆）；软约束——
// 多人同时编辑以后保存为准（并发协作等 P1 git 仓化）。
//
// 后端 /api/v1/film/*（film.rs）并行开发中：接口失败如实展示错误（error-box），
// 不崩；任务统一走 FilmTask 轮询（2s，环形日志尾，完成刷新产物 + 流程页
// refreshTick 联动重载）。
//
// 布局红线：零 vh 公式（窗口内 flex + min-height:0 + overflow 滚动）。
// =============================================================================
import { computed, nextTick, onMounted, onUnmounted, provide, reactive, ref, watch } from 'vue';
import { useI18n } from 'vue-i18n';
import type { CapabilitySnapshot, DegradedState } from '@nexos/app-sdk';
import TimelineTracks from './TimelineTracks.vue';
import PreviewMonitor from './PreviewMonitor.vue';
import { createPreviewEngine, PREVIEW_ENGINE_KEY } from './previewEngine';
// v0.1.7 UI 重设计第一批：design token 三层（light/dark 值域 + @theme 注册 +
// nx-* 组件基座类）。**必须后于 flow.css 引入**——页面在 .nx-page 作用域内以
// 覆写旧共享类，引入顺序即优先级（见 design/theme.css 文件头）。
// v0.1.8 第二批铺满：flow.css 只剩布局骨架（视觉全量迁 theme.css .nx-page 区），
// 全部视图根挂 .nx-page；本组件 scoped 样式同步 token 化（工作室壳/五区/弹窗）。
// 【修复】flow.css import 在第一批改造中意外丢失（0.1.7 构建里 fh-* 布局骨架
// 整体缺失——只有 .nx-page 覆写、无基座规则）——此处恢复，且保持在 theme.css
// 之前引入（引入顺序即优先级）。
import './flow/flow.css';
import './design/theme.css';
import { initNxTheme } from './design/theme';
import NxThemeToggle from './nx/NxThemeToggle.vue';
// v0.1.11 全局操作反馈系统：NxToast 渲染器（挂根模板末尾，Teleport body 右上
// 角堆叠）+ toast 单例 API + 任务↔toast 桥（提交 loading → 轮询进度 → 终态）。
import NxToast from './nx/NxToast.vue';
import { toast } from './nx/toast';
import { createTaskToastNotifier } from './flow/taskToast';
import TaskIndicator from './flow/TaskIndicator.vue';
import SideNav from './flow/SideNav.vue';
import CostBadge from './flow/CostBadge.vue';
import HubLobby from './flow/HubLobby.vue';
// v0.1.45 短片生成首屏（ShotGen：直连 video 渠道单段生成器；默认 mode）。
import ShotGen from './flow/ShotGen.vue';
import { createShotgenEngine, SHOTGEN_ENGINE_KEY } from './flow/shotgen';
import {
  filmShotgenCreate,
  filmShotgenGet,
  filmShotgenList,
} from './api';
import HubBrowse from './flow/HubBrowse.vue';
import StoryPage from './flow/StoryPage.vue';
import StoryboardPage from './flow/StoryboardPage.vue';
import StoryboardGenPanel from './flow/StoryboardGenPanel.vue';
import CastingPage from './flow/CastingPage.vue';
import AudioPage from './flow/AudioPage.vue';
import ComposePage from './flow/ComposePage.vue';
import CollabPage from './flow/CollabPage.vue';
import ModelsPage from './flow/ModelsPage.vue';
import SettingsPage from './flow/SettingsPage.vue';
import { FLOW_CONTEXT_KEY, PROJECT_DEFAULT_KEY, filmTaskStageLabel, type FlowContext } from './flow/flowContext';
import {
  STORY_PIPELINE_STAGES,
  parseChunkProgress,
  type StoryPipelineStage,
} from './flow/pipelineStatus';
import {
  parseStageFromMarkdown,
  presetKeyCap,
  ratioPresetOf,
  RATIO_PRESETS,
  type FilmStage,
  type FlowView,
  type RatioPreset,
} from './flow/flowTypes';
import {
  loadAuthor,
  parseActivity,
  parseOwnership,
  saveAuthor,
  serializeOwnership,
  textToB64,
  type FilmActivityEntry,
  type FilmOwnership,
} from './flow/collab';
import {
  b64ToText,
  bgmEntryKey,
  bgmEntryMood,
  bgmEntryTrigger,
  filmCompose,
  filmCreateCharacter,
  filmCreateProject,
  filmDeleteCharacter,
  filmDeleteProject,
  filmFileDataUrl,
  filmGenPortrait,
  filmGenScript,
  filmGenShotImage,
  filmGenShotTts,
  filmGenShotVideo,
  filmGetFile,
  filmGetProject,
  filmGetProjectModels,
  filmGetTask,
  filmIssueFromTask,
  filmListBgm,
  filmListCasting,
  filmListCharacters,
  filmListFiles,
  filmListProjects,
  filmPutFile,
  filmUpdateCharacter,
  filmUpdateProject,
  filmUploadPortrait,
  filmUploadRef,
  fetchFileDataUrl,
  fetchGatewayChannels,
  fetchLlmInstances,
  readFileAsDataUrl,
  sdkGatewayChannels,
  sdkLlmInstances,
  splitDataUrl,
  hostSdk,
  OPENAI_VOICES,
  type FilmBgmEntry,
  type FilmCastType,
  type FilmCastingObject,
  type FilmCharacter,
  type FilmFileEntry,
  type FilmModelRef,
  type FilmProject,
  type FilmProjectModels,
  type FilmShot,
  type FilmTask,
} from './api';
import {
  distVersionOf,
  distVersions,
  entryMtime,
  fileBasename,
  fmtBytes,
  hubFileEntries,
  parseStandaloneQuery,
} from './flow/flowFiles';

const { t } = useI18n();

// 深浅主题（v0.1.7）：试点页 token 体系随 <html data-theme> 换值；localStorage
// 记忆（缺省浅色）；旧页样式不读 nx 变量——切换零影响。
initNxTheme();

// =============================================================================
// 独立运行外链（右上角图标；仅桌面嵌入模式显示）
// =============================================================================

/** 独立模式标记（apps/film/standalone/standalone-host.ts 置位）——该模式下不显示外链。 */
const isStandalone = Boolean(
  (globalThis as { __NEXOS_STANDALONE__?: boolean }).__NEXOS_STANDALONE__,
);

/** 在新浏览器标签页打开独立全页版本（脱离 NexOS 桌面壳，宿主桥自给自足）。 */
function openStandalone(): void {
  window.open('/apps-assets/film/standalone.html', '_blank', 'noopener');
}

// =============================================================================
// 能力快照与降级三态（@nexos/app-sdk 吃狗粮，v0.1.28）
//
// 装载即 sdk.degraded.state()（内部 capabilities 探测，5s 缓存 + 3 次重试
// 后判 offline）+ 订阅后续变化。应用消费：
//   - 顶栏能力徽章：全能力=无徽章 / degraded=琥珀「部分能力受限」/
//     offline=红「离线模式」（任务口径：独立模式顶栏显徽章）；
//   - 生成按钮按 missing 置灰 + tooltip（chat 缺 llm+gateway / video·tts·
//     music 缺 gateway / compose 缺 media.ffmpeg / offline 全灰）。
// =============================================================================

/** 最近一次能力快照（null=尚无）。 */
const caps = ref<CapabilitySnapshot | null>(null);
/** 降级三态（null=尚未判定）。 */
const deg = ref<DegradedState | null>(null);
/** 能力订阅退订函数（onUnmounted 调）。 */
let unsubCaps: (() => void) | null = null;

/** 某能力键是否缺失（未判定时按不缺失处理——不强置灰）。 */
function capMissing(key: string): boolean {
  return deg.value?.missing.includes(key) ?? false;
}

/** 离线态（探测连败 3 次——全部生成入口停用）。 */
const isOffline = computed(() => deg.value?.mode === 'offline');

/** chat 源是否可用：本地 LLM 或网关渠道至少其一。 */
const chatAvailable = computed(
  () => !isOffline.value && !(capMissing('llm') && capMissing('gateway')),
);
/** 渠道转发类（video/tts/music）是否可用。 */
const channelAvailable = computed(() => !isOffline.value && !capMissing('gateway'));
/** 成片合成是否可用（ffmpeg）。 */
const composeAvailable = computed(() => !isOffline.value && !capMissing('media.ffmpeg'));

/** 生成按钮 tooltip（按缺失组合给文案；不缺失返回 undefined=不显）。 */
function genDisabledTip(kind: 'script' | 'media' | 'compose'): string | undefined {
  if (!deg.value || deg.value.mode === 'full') return undefined;
  if (deg.value.mode === 'offline') return t('film.capsOfflineTip');
  if (kind === 'compose' && !composeAvailable.value) {
    return t('film.capsMissingTip', { caps: 'media.ffmpeg' });
  }
  if (kind === 'script' && !chatAvailable.value) {
    return t('film.capsMissingTip', { caps: ['llm', 'gateway'].join(', ') });
  }
  if (kind === 'media' && !channelAvailable.value) {
    return t('film.capsMissingTip', { caps: 'gateway' });
  }
  return undefined;
}

/** 启动能力判定（SDK 在桥上才启用；旧宿主静默跳过=无徽章全功能）。 */
async function initCaps(): Promise<void> {
  const sdk = hostSdk();
  if (!sdk) return;
  try {
    deg.value = await sdk.degraded.state();
    caps.value = sdk.capabilities.cached();
  } catch {
    /* degraded.state 不抛错（offline 三态收敛）；防御旧宿主形态 */
  }
  unsubCaps = sdk.capabilities.subscribe((s) => {
    caps.value = s;
  });
}

// —— 分辨率预设（v0.1.37 六档；表与 helpers 见 flow/flowTypes.ts）——
// 预设卡名称/说明 i18n 键：film.preset<Key> / film.preset<Key>Desc。

/** 预设名称（未知档回退原始比例字符串——老项目数据兜底）。 */
function presetName(ratio?: string | null): string {
  if (!ratio) return ''
  const p = ratioPresetOf(ratio)
  return p ? t(`film.preset${presetKeyCap(p.key)}`) : ratio
}

/** 预设说明（用途一行；未知档空串）。 */
function presetDesc(ratio?: string | null): string {
  if (!ratio) return ''
  const p = ratioPresetOf(ratio)
  return p ? t(`film.preset${presetKeyCap(p.key)}Desc`) : ''
}

/** 预设合成分辨率文案（1920×1080；未知档空串）。 */
function presetRes(ratio?: string | null): string {
  const p = ratio ? ratioPresetOf(ratio) : undefined
  return p ? `${p.width}×${p.height}` : ''
}

/** 预设卡图示比例条的 CSS（按数值比例钳制在 44×32 盒内画 mini 条）。 */
function presetBarStyle(p: RatioPreset): Record<string, string> {
  const r = p.width / p.height
  // 宽条：宽撑满、高按比例（下限 6px 防超宽档不可见）；竖条对称
  const w = r >= 1 ? 44 : Math.max(6, Math.round(44 * r))
  const h = r >= 1 ? Math.max(6, Math.round(44 / r)) : 32
  return { width: `${w}px`, height: `${h}px` }
}

/** 能力面（与 FilmModelRef.capability 一致）。 */
type Cap = FilmModelRef['capability'];

// =============================================================================
// 模型源（下拉数据）：本地实例 + 网关渠道
// =============================================================================

/** LLM 实例轻量视图（GET /api/v1/llm/instances 元素子集，宽松字段）。 */
interface LlmInstLite {
  id?: string;
  name?: string;
  model?: string;
  status?: string;
  config?: { served_model_name?: string | null } | null;
  [k: string]: unknown;
}

/** 网关渠道轻量视图（GET /api/v1/gateway/channels 元素子集，宽松字段）。 */
interface ChLite {
  id?: string;
  name?: string;
  provider?: string;
  enabled?: boolean;
  status?: string;
  /** 渠道模型清单（ShotGen 模型标识拼接用；可缺省）。 */
  models?: string[];
  /** 联邦中继来源 NodeID（非空 = 🌐 中继渠道）。 */
  via_node?: string;
  [k: string]: unknown;
}

const llmInstances = ref<LlmInstLite[]>([]);
const channels = ref<ChLite[]>([]);
/** 模型源加载错误（不阻断页面——只是下拉里缺对应组）。 */
const srcError = ref('');
/** 模型源首载完成（ShotGen 黄条只在加载完成后判定——防首帧闪「未配置」）。 */
const modelSourcesLoaded = ref(false);

/** 运行中的本地 LLM 实例（chat 源）。 */
const runningLlms = computed(() =>
  llmInstances.value.filter((i) => (i.status ?? '') === 'running'),
);

/** 启用中的网关渠道（与 ApiGateway channelStatusLabel 同口径）。 */
const enabledChannels = computed(() =>
  channels.value.filter(
    (c) => (c.status ?? (c.enabled ? 'enabled' : 'disabled')) === 'enabled',
  ),
);

/** 实例展示名：name > served_model_name > model > 兜底。 */
function instanceLabel(i: LlmInstLite): string {
  return (
    i.name || i.config?.served_model_name || i.model || t('film.localLlm')
  );
}

/** 下拉选项（单组内）。relay=true 时选项尾部带 🌐 中继徽章。 */
interface ModelOption {
  key: string;
  label: string;
  relay: boolean;
}
/** 下拉分组（optgroup）。 */
interface ModelGroup {
  label: string;
  options: ModelOption[];
}

/**
 * 按能力面构造下拉分组：
 * - chat：[本地 LLM 实例(running)] ∪ [网关渠道]；本地选项 key=llm:<id>，但契约
 *   model_ref 无本地实例 id 字段（source:local 即可，后端取默认运行实例）——
 *   多实例并列展示仅为可读性。
 * - image：[本地 sd-turbo] ∪ [网关渠道]
 * - video/tts/music：仅 [网关渠道]（本地无这些能力）
 */
function optionsFor(cap: Cap): ModelGroup[] {
  const groups: ModelGroup[] = [];
  if (cap === 'chat') {
    groups.push({
      label: t('film.grpLocal'),
      options: runningLlms.value.map((i, idx) => ({
        key: `llm:${i.id ?? i.name ?? idx}`,
        label: instanceLabel(i),
        relay: false,
      })),
    });
  } else if (cap === 'image') {
    groups.push({
      label: t('film.grpLocal'),
      options: [{ key: 'local', label: t('film.localSd'), relay: false }],
    });
  }
  groups.push({
    label: t('film.grpChannel'),
    options: enabledChannels.value.map((c) => ({
      key: `ch:${c.id ?? ''}`,
      label: c.name || c.id || c.provider || '?',
      relay: !!c.via_node,
    })),
  });
  return groups.filter((g) => g.options.length > 0);
}

/** 某能力面是否有可选项。 */
function hasOptionsFor(cap: Cap): boolean {
  return optionsFor(cap).some((g) => g.options.length > 0);
}

/** 各能力面选中的下拉 key（'' = 未选；local / llm:<id> / ch:<channel_id> /
 *  project=「项目默认」v0.1.38——选中即不传 model_ref，走 models.json 缺省链）。 */
const modelSel = reactive<Record<Cap, string>>({
  chat: '',
  image: '',
  video: '',
  tts: '',
  music: '',
});

/** 下拉 key → 契约 model_ref（无效选择 / 「项目默认」返回 null——后者语义为
 *  **不传 model_ref 字段**，调用方以 isProjectDefaultSel 区分两者）。 */
function modelRefFor(cap: Cap): FilmModelRef | null {
  const key = modelSel[cap];
  if (!key || key === PROJECT_DEFAULT_KEY) return null;
  if (key === 'local' || key.startsWith('llm:')) {
    return { source: 'local', capability: cap };
  }
  if (key.startsWith('ch:')) {
    const channelId = key.slice(3);
    if (!channelId) return null;
    return { source: 'channel', channel_id: channelId, capability: cap };
  }
  return null;
}

// —— 项目级模型设置（v0.1.38：models.json 八能力位默认源；「项目默认」项数据） ——

/** models 配置快照（GET :id/models；null=未加载/后端未就绪——摘要降级为未设置）。 */
const projectModels = ref<FilmProjectModels | null>(null);

/** 刷新 models 配置快照（失败抛出——调用方自决吞/显）。 */
async function reloadProjectModels(): Promise<void> {
  const cur = project.value;
  if (!cur) return;
  projectModels.value = await filmGetProjectModels(cur.id);
}

/** 能力位项目默认摘要（「项目默认」选项/小字显示；''=未设置）。 */
function defaultModelSummary(cap: string): string {
  const entry = projectModels.value?.capabilities?.find((c) => c.capability === cap);
  if (!entry?.source) return '';
  if (entry.source === 'local') return t('models.sourceLocal');
  const model = (entry.model || '').trim();
  const chName =
    channels.value.find((c) => c.id === entry.channel_id)?.name || entry.channel_id || '?';
  return model ? `${chName} · ${model}` : String(chName);
}

/** 是否选中「项目默认」（生成时不传 model_ref 字段）。 */
function isProjectDefaultSel(cap: Cap): boolean {
  return modelSel[cap] === PROJECT_DEFAULT_KEY;
}

/** 能力面下拉是否就绪（有可选项或选中「项目默认」——生成按钮置灰口径放宽）。 */
function modelSelReady(cap: Cap): boolean {
  return hasOptionsFor(cap) || isProjectDefaultSel(cap);
}

/** 模型源加载（失败仅记 srcError，不阻断页面）。优先 @nexos/app-sdk
 * （sdk.llm.instances / sdk.gateway.channels），旧宿主无桥 sdk 时回退
 * 手拼端点（api.ts fetch 版）。 */
async function loadModelSources(): Promise<void> {
  srcError.value = '';
  const errs: string[] = [];
  // 本地实例（chat 源）：失败时下拉只剩渠道组
  try {
    const viaSdk = await sdkLlmInstances();
    const raw = viaSdk ?? (await fetchLlmInstances());
    llmInstances.value = Array.isArray(raw) ? (raw as LlmInstLite[]) : [];
  } catch (e) {
    llmInstances.value = [];
    errs.push(`LLM 实例：${errMsg(e)}`);
  }
  // 网关渠道（全部能力面的渠道源；via_node 非空 = 🌐 联邦中继）
  try {
    const viaSdk = await sdkGatewayChannels();
    const raw = viaSdk ?? (await fetchGatewayChannels());
    channels.value = Array.isArray(raw) ? (raw as ChLite[]) : [];
  } catch (e) {
    channels.value = [];
    errs.push(`网关渠道：${errMsg(e)}`);
  }
  if (errs.length) srcError.value = errs.join('；');
  modelSourcesLoaded.value = true;
  pickDefaultModels();
}

/** 首次进入时为各能力面挑默认项（已有选择不动）。 */
function pickDefaultModels(): void {
  if (!modelSel.chat) {
    const first = optionsFor('chat')
      .flatMap((g) => g.options)
      .find((o) => o.key.startsWith('llm:'));
    if (first) modelSel.chat = first.key;
  }
  if (!modelSel.image) modelSel.image = 'local';
  for (const cap of ['video', 'tts', 'music'] as Cap[]) {
    if (modelSel[cap]) continue;
    const first = optionsFor(cap)
      .flatMap((g) => g.options)
      .find((o) => o.key.startsWith('ch:'));
    if (first) modelSel[cap] = first.key;
  }
}

// =============================================================================
// 三态视图（v0.1.45）：shotgen 短片生成首屏（默认）/ list FilmHub 大厅 /
// studio 项目工作室。全流程（大厅+工作室）=「高级模式」——ShotGen 顶栏进入；
// 工作室返回按来路回 shotgen 或 list（studioReturnMode 记忆）。
// =============================================================================

/** 视图三态：shotgen 短片生成（默认首屏）/ list FilmHub 大厅 / studio 工作室。 */
const mode = ref<'shotgen' | 'list' | 'studio'>('shotgen');

/** 工作室返回目标（enterStudio 时按来路记忆；「← 返回」与 SideNav 🎬 共用）。 */
const studioReturnMode = ref<'shotgen' | 'list'>('list');

const projects = ref<FilmProject[]>([]);
const listLoading = ref(false);
const listError = ref('');

// =============================================================================
// 短片生成引擎（v0.1.45 ShotGen）：FilmStudio 创建并 provide——跨模式切换
// 存活（切高级模式/进工作室再返回，提交与轮询不中断）。toast 静默口径：
// 引擎不进任务中心不落全局 toast，反馈全在 ShotGen 页内（作品状态/红条）。
// =============================================================================

const shotgenEngine = createShotgenEngine({
  create: filmShotgenCreate,
  list: filmShotgenList,
  get: filmShotgenGet,
  resolveMedia: fetchFileDataUrl,
  errMsg,
});
provide(SHOTGEN_ENGINE_KEY, shotgenEngine);

/** ShotGen 当前模型标识（video 位渠道名 + 首个模型；''=未配置）。 */
const sgModelName = computed(() => {
  const c = enabledChannels.value[0];
  if (!c) return '';
  const model = (c.models ?? [])[0];
  const name = String(c.name || c.id || '?');
  return model ? `${name} · ${model}` : name;
});

/** video 位是否可用（非离线 + 网关可达 + 有启用渠道；黄条/按钮置灰口径）。
 *  模型源未首载完成前按可用处理（防首帧误闪「未配置」黄条）。 */
const sgVideoReady = computed(
  () =>
    modelSourcesLoaded.value &&
    channelAvailable.value &&
    enabledChannels.value.length > 0,
);

/** ShotGen「高级模式 →」：进现有 FilmHub 大厅（全流程入口）。 */
function goShotgenAdvanced(): void {
  mode.value = 'list';
}

/** ShotGen「去配置」：跳模型设置（项目级 video 能力位）——有项目直进最近
 *  项目的模型设置页（来路记忆 shotgen：返回时回短片生成）；无项目进大厅。 */
function goShotgenModels(): void {
  const latest = [...projects.value].sort(
    (a, b) => Date.parse(b.updated_at ?? '') - Date.parse(a.updated_at ?? ''),
  )[0];
  if (latest) {
    studioReturnMode.value = 'shotgen';
    enterStudio(latest, 'models');
  } else {
    mode.value = 'list';
  }
}

async function loadProjects(): Promise<void> {
  listLoading.value = true;
  listError.value = '';
  try {
    const raw = await filmListProjects();
    projects.value = Array.isArray(raw) ? raw : [];
  } catch (e) {
    projects.value = [];
    listError.value = errMsg(e);
  } finally {
    listLoading.value = false;
  }
}

async function removeProject(p: FilmProject): Promise<void> {
  if (!window.confirm(t('film.delConfirm', { title: p.title }))) return;
  try {
    await filmDeleteProject(p.id);
    if (project.value?.id === p.id) {
      mode.value = 'list';
      project.value = null;
    }
    await loadProjects();
    // v0.1.11 危险确认类：删除成功全局可见（此前仅列表刷新——静默）
    toast.success(t('toast.deleted'));
  } catch (e) {
    window.alert(t('film.delFailed') + errMsg(e));
  }
}

// —— 新建项目对话框（三段式钉底弹窗）——
const showCreate = ref(false);
const creating = ref(false);
const createError = ref('');
const createForm = reactive({
  title: '',
  idea: '',
  ratio: '16:9',
  style_hint: '',
});

function openCreate(): void {
  createForm.title = '';
  createForm.idea = '';
  createForm.ratio = '16:9';
  createForm.style_hint = '';
  createError.value = '';
  showCreate.value = true;
}

async function submitCreate(): Promise<void> {
  createError.value = '';
  if (!createForm.title.trim()) {
    createError.value = t('film.errTitle');
    return;
  }
  if (!createForm.idea.trim()) {
    createError.value = t('film.errIdea');
    return;
  }
  creating.value = true;
  try {
    const p = await filmCreateProject({
      title: createForm.title.trim(),
      idea: createForm.idea.trim(),
      ratio: createForm.ratio,
      style_hint: createForm.style_hint.trim() || undefined,
    });
    showCreate.value = false;
    await loadProjects();
    // v0.1.11 创建成功全局反馈（弹窗关闭 + 跳转本身可见，补 toast 闭环）
    toast.success(t('toast.created', { name: p.title }));
    enterStudio(p);
  } catch (e) {
    createError.value = t('film.createFailed') + errMsg(e);
  } finally {
    creating.value = false;
  }
}

// =============================================================================
// 项目工作室
// =============================================================================

const project = ref<FilmProject | null>(null);
const projectLoading = ref(false);
const projectError = ref('');
/** 当前选中镜头序号（1 起）。 */
const selectedShot = ref(1);

const shots = computed<FilmShot[]>(() => project.value?.script ?? []);
const selShot = computed<FilmShot | null>(
  () => shots.value.find((s) => s.shot === selectedShot.value) ?? null,
);

// —— 预览播放引擎（v0.1.35）：监视器（PreviewMonitor）与时间轴播放头
//    （TimelineTracks）经 provide/inject 共享——播放头双向：时间轴点击/拖动 →
//    seek；引擎播放 → playheadSec 驱动时间轴头移动。loader = files/download
//    b64 信封 → data URL（与缩略图同口径），产物目录随项目态取。
const previewEngine = createPreviewEngine({
  loader: (name) => {
    const dir = project.value?.dir?.replace(/\/$/, '');
    if (!dir) return Promise.reject(new Error('项目产物目录未就绪'));
    return fetchFileDataUrl(`${dir}/${name}`);
  },
});
provide(PREVIEW_ENGINE_KEY, previewEngine);

/** 项目态 → 引擎源同步（任务终态 reloadProject / 编辑保存后自动重载段表）。 */
watch(
  () => [project.value?.script, project.value?.artifacts, project.value?.ratio] as const,
  () => {
    previewEngine.setSources({
      shots: shots.value,
      artifactNames: (project.value?.artifacts ?? []).map((a) => a.name),
      ratio: project.value?.ratio ?? '16:9',
      finalAvailable: hasArtifact('final.mp4'),
      bgmAvailable: hasArtifact('bgm.mp3'),
    });
  },
  { immediate: true, deep: true },
);

/** 成片整播切换（「预览成片」钮：监视器切 final 模式整播 final.mp4）。 */
const previewingFinal = computed(() => previewEngine.mode.value === 'final');

function toggleFinalPreview(): void {
  // 工作台口径固定默认 final.mp4（合成页版本预览可能改过 finalName——复位）
  if (previewEngine.mode.value !== 'final') previewEngine.setFinalName('final.mp4');
  previewEngine.setMode(previewEngine.mode.value === 'final' ? 'storyboard' : 'final');
}

// =============================================================================
// FilmHub 流程化（v0.1.35）：左侧选项卡视图态 + FlowContext（provide 给流程页）
// + README 阶段 + 多人分工（ownership/activity/author）。工作台（原五区）为
// 选项卡之一——「去工作台细调」/「预览成片」经 setView 跳转。
// =============================================================================

/** 当前选项卡视图（story/storyboard/casting/audio/compose/workbench/settings）。 */
const navView = ref<FlowView>('workbench');
/** 用户已手动点击导航（此后 loadStage 回调不再抢视图）。 */
const navUserPicked = ref(false);
/** README frontmatter 阶段（''=未知——导航徽章退化）。 */
const flowStage = ref<FilmStage | ''>('');
/** 数据刷新版本号（任务终态 ++；流程页 watch 后重载自己的数据）。 */
const refreshTick = ref(0);
// —— 多人分工 v1 ——
/** ownership.json（成员 + 分区认领 + 定妆对象认领）。 */
const ownership = ref<FilmOwnership | null>(null);
/** 当前操作人（「我是」；写操作 author 字段）。 */
const flowAuthor = ref(loadAuthor());
/** 活动流（activity.json 最近条目，新→旧）。 */
const activity = ref<FilmActivityEntry[]>([]);
/** Hub 浏览「在工作台打开」待选中的定妆对象（定妆页消费后清空）。 */
const pendingCastSelect = ref<{ type: FilmCastType; name: string } | null>(null);
/** SideNav 树卡点击的待打开 Hub 文件路径（v0.1.37.1 注册表式：HubBrowse
 *  挂载/更新时消费并清空——选中该文件并加载内容）。 */
const pendingHubFile = ref<string | null>(null);
/** 剧情页章节卡「从此章生成分镜」待预填的章节范围（v0.1.44：StoryboardGenPanel
 *  挂载/更新时消费并清空——下拉选中该章 + 展开面板）。 */
const pendingStoryboardChapter = ref<string | null>(null);

function setFlowView(v: FlowView): void {
  navUserPicked.value = true;
  navView.value = v;
}

/** 成员下拉（「我是」选择器 options；含当前值去重保序）。 */
const memberOptions = computed<string[]>(() => {
  const list = ownership.value?.members ?? [];
  return list.includes(flowAuthor.value) || flowAuthor.value === 'anonymous'
    ? list
    : [flowAuthor.value, ...list];
});

function onAuthorChange(e: Event): void {
  flowAuthor.value = saveAuthor((e.target as HTMLSelectElement).value);
}

/** 读 README 阶段（frontmatter stage；README.md → project.md 两路径尝试）。 */
async function loadStage(): Promise<void> {
  const cur = project.value;
  if (!cur) return;
  for (const p of ['README.md', 'project.md']) {
    try {
      const env = await filmGetFile(cur.id, p);
      const b64 = env.content_b64 ?? '';
      if (!b64) continue
      const stage = parseStageFromMarkdown(b64ToText(b64));
      if (stage) {
        flowStage.value = stage;
        return;
      }
    } catch {
      /* 文件缺失/后端未就绪——下一路径 */
    }
  }
  // 兜底：README 缺失的旧项目按本地态推导（有分镜=storyboard，否则=story）
  flowStage.value = (cur.script ?? []).length ? 'storyboard' : 'story';
}

/** 刷新协作态（ownership.json + activity.json；写操作后/任务终态调用）。 */
async function refreshCollab(): Promise<void> {
  const cur = project.value;
  if (!cur) return;
  const [ownEnv, actEnv] = await Promise.all([
    filmGetFile(cur.id, 'ownership.json').catch(() => null),
    filmGetFile(cur.id, 'activity.json').catch(() => null),
  ]);
  ownership.value = parseOwnership(ownEnv);
  activity.value = parseActivity(actEnv);
}

/** 保存 ownership（PUT files/ownership.json 带作者；成功更新本地态）。
 *  v0.1.11：成败全局 toast（认领/成员/分区等全部保存点经此统一反馈——
 *  此前成功静默、失败 window.alert 阻塞）。 */
async function saveOwnership(next: FilmOwnership): Promise<boolean> {
  const cur = project.value;
  if (!cur) return false;
  try {
    await filmPutFile(
      cur.id,
      'ownership.json',
      textToB64(serializeOwnership(next)),
      flowAuthor.value,
    );
    ownership.value = next;
    toast.success(t('toast.saved'));
    return true;
  } catch (e) {
    toast.error(t('film.ownSaveFailed') + errMsg(e));
    return false;
  }
}

/** 任务中心条目（tracked = 已进入 UI 的任务）。 */
interface TrackedTask {
  id: string;
  kind: string;
  shot: number | null;
  status: string;
  lastLog: string;
  /** 环形日志尾（v0.1.7：分块进度「块 X/Y」解析输入）。 */
  logTail: string[];
  error: string;
  /** 关联源文件（v0.1.7：story 管线任务源行聚合徽章）。 */
  sourceFile: string;
  /** 创建时刻（ms epoch；v0.1.7：运行态已耗时口径）。 */
  createdAt: number | null;
  /** 终态（completed/failed）——不再轮询。 */
  done: boolean;
  /** 产物路径（v0.1.11：终态通知摘要解析输入）。 */
  output: string | null;
  /** 提交时持有的 loading toast id（v0.1.11 闭环：终态翻 success/error）。 */
  toastId: number | null;
}

const trackedTasks = ref<TrackedTask[]>([]);
const POLL_MS = 2000;
let pollTimer: ReturnType<typeof setInterval> | null = null;

// —— v0.1.11 全局操作反馈：任务↔toast 桥 + 顶栏全局任务指示器 ——
//    提交 loading → 轮询进度文案（「向量化中 3/17」）→ 终态 success/error
//    （一次操作完整闭环）；失败 toast 带「查看任务」动作跳工作台任务中心。

/** 跳工作台并展开任务中心（失败 toast 动作钮 / 指示器「查看全部」共用；
 *  在大厅点入时回工作室壳——任务归属项目态仍在）。 */
function goWorkbenchTasks(): void {
  if (mode.value === 'list' && project.value) mode.value = 'studio';
  setFlowView('workbench');
  tasksOpen.value = true;
}

/** 任务通知器（提交/进度/终态 toast；t 随语言切换实时取值）。 */
const taskNotifier = createTaskToastNotifier(t, goWorkbenchTasks);

// —— 紧凑任务条（左栏底部；任务中心 v0.1.35 从右栏迁来，功能不丢：
//    收起=一行「任务 N 进行中」摘要，展开=完整列表 + 环形日志尾）——
const tasksOpen = ref(false);
/** 进行中（非终态）任务数。 */
const activeTaskCount = computed(() => trackedTasks.value.filter((x) => !x.done).length);

/** 顶栏全局任务指示器行投影（v0.1.11：标签 + 已耗时 + X/Y 分块进度）。 */
const indicatorTasks = computed(() =>
  trackedTasks.value
    .filter((x) => !x.done)
    .map((x) => {
      const counts = parseChunkProgress(x.logTail);
      return {
        id: x.id,
        label: taskLabel(x),
        createdAt: x.createdAt,
        xy: counts ? `${counts.done}/${counts.total}` : '',
      };
    }),
);

/** created_at 宽容归一（number | ISO 串 → ms epoch；无效 null）。 */
function taskCreatedAt(v: number | string | undefined | null): number | null {
  if (typeof v === 'number' && Number.isFinite(v)) return v;
  if (typeof v === 'string' && v) {
    const ms = Date.parse(v);
    if (!Number.isNaN(ms)) return ms;
  }
  return null;
}

function addTracked(task: FilmTask): void {
  // 后端字段是 stage（kind 为旧契约别名）——202 响应即取，防 undefined 渲染崩
  const kind = (task as unknown as { stage?: string }).stage ?? task.kind ?? '';
  const entry: TrackedTask = {
    id: task.id,
    kind,
    shot: null,
    status: task.status,
    lastLog: (task.log ?? [])[0] ?? '',
    logTail: [...(task.log ?? [])],
    error: task.error ?? '',
    sourceFile: '',
    createdAt: taskCreatedAt(task.created_at),
    done: task.status === 'completed' || task.status === 'failed',
    output: task.output ?? null,
    toastId: null,
  };
  // v0.1.11 闭环第一环：提交即 loading toast（终态在 pollTasks 翻 success/
  // error；后端映射 done/error → completed/failed 的归一在 pollTasks 侧）
  const status = task.status === 'done' ? 'completed' : task.status === 'error' ? 'failed' : task.status;
  entry.done = status === 'completed' || status === 'failed';
  if (!entry.done) {
    entry.toastId = taskNotifier.submitted(kind);
  } else {
    // 极端：202 即终态（秒级任务）——不进轮询，直接发终态通知（toastId=0
    // 失效 → notifier 内部重落一条，终态不丢）
    taskNotifier.terminal(0, {
      id: entry.id,
      stage: kind,
      shot: null,
      status,
      logTail: entry.logTail,
      output: entry.output,
      error: entry.error,
    });
  }
  trackedTasks.value.unshift(entry);
  startPolling();
}

/**
 * 管线任务统一进任务中心（v0.1.6）：管线按钮（story/clean·chapterize·profile·
 * embed / story.generate / storyboard.generate / casting extract）202 后以
 * id+stage 显式登记——stage 是前端常量，不依赖 202 响应回带；同 id 去重防
 * 重复登记。轮询 / 终态 refreshTick 联动与 addTracked 同一条路。
 * v0.1.7 meta：sourceFile（源行聚合）+ createdAt（已耗时）。
 */
function trackFilmTask(
  id: string,
  stage: string,
  meta?: { sourceFile?: string; createdAt?: number | null },
): void {
  if (!id || trackedTasks.value.some((x) => x.id === id)) return;
  // v0.1.11 闭环第一环：提交即 loading toast（「已提交，完成后通知」）
  const toastId = taskNotifier.submitted(stage);
  trackedTasks.value.unshift({
    id,
    kind: stage,
    shot: null,
    status: 'queued',
    lastLog: '',
    logTail: [],
    error: '',
    sourceFile: meta?.sourceFile ?? '',
    createdAt: meta?.createdAt ?? null,
    done: false,
    output: null,
    toastId,
  });
  startPolling();
}

/** 剧情管线任务快照（v0.1.7 FlowContext.storyTasks：源行聚合徽章数据源）。 */
const storyTasks = computed(() =>
  trackedTasks.value
    .filter(
      (x) =>
        STORY_PIPELINE_STAGES.includes(x.kind as StoryPipelineStage) && !!x.sourceFile,
    )
    .map((x) => ({
      id: x.id,
      stage: x.kind,
      status: x.status,
      sourceFile: x.sourceFile,
      log: x.logTail,
      error: x.error,
      createdAt: x.createdAt,
    })),
);

/** FlowContext（流程页注入消费：项目/模型源/任务中心/阶段/协作态）。 */
const flowCtx: FlowContext = {
  project,
  optionsFor,
  hasOptionsFor,
  modelSel,
  modelRefFor,
  addTracked,
  trackFilmTask,
  storyTasks,
  errMsg,
  refreshTick,
  reloadProject,
  chatAvailable,
  channelAvailable,
  composeAvailable,
  isOffline,
  stage: flowStage,
  view: navView,
  setView: setFlowView,
  ownership,
  author: flowAuthor,
  activity,
  setAuthor: (name: string) => {
    flowAuthor.value = saveAuthor(name);
  },
  saveOwnership,
  refreshCollab,
  pendingCastSelect,
  pendingHubFile,
  pendingStoryboardChapter,
  // —— 项目级模型设置（v0.1.38）——
  projectModels,
  reloadProjectModels: async () => {
    try {
      await reloadProjectModels();
    } catch {
      projectModels.value = null; // 后端未就绪：摘要降级为「未设置」不崩
    }
  },
  defaultModelSummary,
  modelSelReady,
  isProjectDefaultSel,
};
provide(FLOW_CONTEXT_KEY, flowCtx);

/**
 * 进入项目工作室（大厅卡「打开」缺省按 README 阶段定缺省页；「Hub 浏览」
 * 传 view='hub' 直达文件树浏览——navUserPicked 置位防 loadStage 抢视图）。
 */
function enterStudio(p: FilmProject, view?: FlowView): void {
  mode.value = 'studio';
  // 来路记忆复位：默认回大厅（goShotgenModels 等 shotgen 来路在调用后覆写）
  studioReturnMode.value = 'list';
  project.value = p;
  projectError.value = '';
  selectedShot.value = 1;
  draftDirty.value = false;
  fillDraft();
  navUserPicked.value = !!view;
  navView.value = view ?? 'workbench';
  flowStage.value = '';
  ownership.value = null;
  activity.value = [];
  projectModels.value = null;
  pendingCastSelect.value = null;
  pendingHubFile.value = null;
  pendingStoryboardChapter.value = null;
  flowAuthor.value = loadAuthor();
  void reloadProject();
  void loadCharacters();
  void loadCastActions();
  // v0.1.38 项目级模型配置快照（「项目默认」下拉项摘要；旧后端 404 降级不崩）
  void reloadProjectModels().catch(() => {
    projectModels.value = null;
  });
  // 进入项目即见左侧选项卡布局：读 README 阶段定缺省页（未知时：有分镜回
  // 工作台、无分镜进剧情页——产品流程 hub 建项目 → 剧情页）；用户已点击则不抢
  void loadStage().then(() => {
    if (navUserPicked.value) return;
    if (flowStage.value) navView.value = flowStage.value;
    else navView.value = shots.value.length ? 'workbench' : 'story';
  });
  void refreshCollab();
}

/** 大厅卡「Hub 浏览」：进项目并直达文件树浏览视图。 */
function browseProject(p: FilmProject): void {
  enterStudio(p, 'hub');
}

/** SideNav 树卡点击文件（v0.1.37.1 注册表式）：跳 Hub 浏览页并在网页主区
 *  打开该文件（pendingHubFile → HubBrowse 消费定位）。 */
function onNavTreeFileClick(path: string): void {
  pendingHubFile.value = path;
  setFlowView('hub');
}

// —— 视图深链（v0.1.5）——
// URL 约定：standalone.html?p=<projectId>&view=<viewKey>（viewKey ∈
// story|storyboard|casting|audio|compose|workbench|hub|models|settings）；
// casting 可加 &cast=<type>/<name>（定妆页挂载消费选中对象）、hub 可加
// &file=<path>（HubBrowse 挂载消费打开该文件）。启动（onMounted）解析
// location.search：有 p 且项目存在 → 直接 enterStudio(项目, view) 跳过大厅；
// 无 p / 项目不存在 → 大厅照旧。嵌入桌面模式地址栏是宿主壳（?app=film 无
// p）——解析无副作用。多人协作：把深链发给同僚即可在浏览器直达同一视图
// （同源 token 共享天然可用）。

/** 启动深链（location.search 一次性快照；SideNav ⧉ 外链 / 手拼 URL 均可生成）。 */
const bootDeepLink = parseStandaloneQuery(
  typeof window !== 'undefined' ? window.location.search : '',
);

/** 项目列表就绪后应用深链（项目不存在 → 留在大厅，不额外报错）。 */
function applyBootDeepLink(): void {
  if (!bootDeepLink.projectId) return;
  const p = projects.value.find((x) => x.id === bootDeepLink.projectId);
  if (!p) return;
  enterStudio(p, bootDeepLink.view || undefined);
  // 定位参数在 enterStudio（内部清空 pending*）之后置位——由对应页面挂载消费
  if (bootDeepLink.cast) pendingCastSelect.value = bootDeepLink.cast;
  if (bootDeepLink.file) pendingHubFile.value = bootDeepLink.file;
}

async function reloadProject(): Promise<void> {
  const cur = project.value;
  if (!cur || projectLoading.value) return;
  projectLoading.value = true;
  try {
    const fresh = await filmGetProject(cur.id);
    // 保守合并：响应缺字段时保留旧值（后端字段并行开发中，宽松容忍）
    project.value = {
      ...cur,
      ...fresh.project,
      script: fresh.script ?? cur.script,
      artifacts: fresh.artifacts ?? cur.artifacts,
      refs: fresh.refs ?? cur.refs,
    };
    projectError.value = '';
    // 选中镜头被删（重生成剧本序号变化）时回退到首个镜头
    if (!selShot.value && shots.value.length) {
      selectedShot.value = shots.value[0].shot;
      draftDirty.value = false;
    }
    // 草稿有未保存编辑时不回填（避免轮询刷新覆写用户输入）
    if (!draftDirty.value) fillDraft();
  } catch (e) {
    projectError.value = t('film.projectFailed') + errMsg(e);
  } finally {
    projectLoading.value = false;
  }
}

// —— 镜头编辑草稿（选中镜头面板；dirty 期间轮询刷新不回填，避免覆写输入）——
const draft = reactive({
  description: '',
  image_prompt: '',
  video_prompt: '',
  line: '',
  duration_secs: null as number | null,
});
const draftDirty = ref(false);
const savingShot = ref(false);
const saveMsg = ref('');

function fillDraft(): void {
  const s = selShot.value;
  draft.description = s?.desc ?? '';
  draft.image_prompt = s?.image_prompt ?? '';
  draft.video_prompt = s?.video_prompt ?? '';
  draft.line = s?.line ?? '';
  draft.duration_secs =
    typeof s?.duration_secs === 'number' && s.duration_secs > 0
      ? s.duration_secs
      : null;
}

watch(selectedShot, () => {
  draftDirty.value = false;
  saveMsg.value = '';
  fillDraft();
});

// —— 底部多轨时间轴联动（TimelineTracks 块点击=选中镜头；反向：左侧镜头卡 /
//    时间轴选中变化时轨道区滚入对应块——组件内自理，此处只负责滚面板到选中）——
const shotPanelEl = ref<HTMLElement | null>(null);

function onTimelineSelect(n: number): void {
  selectedShot.value = n;
  void nextTick(() => {
    // 窄屏堆叠布局下把镜头面板滚进视口；宽屏面板本身在视口内（nearest 无位移）
    shotPanelEl.value?.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
  });
}

function markDirty(): void {
  draftDirty.value = true;
  saveMsg.value = '';
}

/** 保存镜头（PUT script 局部：只带 shot + 编辑字段，后端按镜头号合并）。 */
async function saveShot(): Promise<void> {
  const cur = project.value;
  const n = selectedShot.value;
  if (!cur || !selShot.value) return;
  savingShot.value = true;
  saveMsg.value = '';
  try {
    const fresh = await filmUpdateProject(cur.id, {
      script: [
        {
          shot: n,
          desc: draft.description,
          image_prompt: draft.image_prompt,
          video_prompt: draft.video_prompt,
          line: draft.line,
          duration_secs: draft.duration_secs ?? undefined,
        },
      ],
    });
    if (fresh.script) project.value = { ...cur, script: fresh.script };
    draftDirty.value = false;
    saveMsg.value = t('film.shotSaved');
    toast.success(t('toast.saved'));
  } catch (e) {
    saveMsg.value = t('film.saveFailed') + errMsg(e);
    toast.error(t('film.saveFailed') + errMsg(e));
  } finally {
    savingShot.value = false;
  }
}

// —— 镜头状态图标（按产物清单判断；时间轴卡 + 面板徽章共用口径）——
/** 产物文件是否存在（GET 项目详情 artifacts 清单）。 */
function hasArtifact(name: string): boolean {
  return (project.value?.artifacts ?? []).some((a) => a.name === name);
}

function shotState(s: FilmShot): { icon: string; label: string } {
  if (hasArtifact(`shot-${s.shot}.mp4`)) return { icon: '▶', label: t('film.stVideo') };
  if (hasArtifact(`line-${s.shot}.mp3`)) return { icon: '🔊', label: t('film.stTts') };
  if (hasArtifact(`shot-${s.shot}.png`)) return { icon: '🖼', label: t('film.stImage') };
  return { icon: '📝', label: t('film.stPending') };
}

// —— 生成动作（分镜 / 图 / 视频 / 配音 / 合成）——
// （BGM 生成入口已移音频页——工作台合成区为纯总装形态，v0.1.36）
// v0.1.39：「生成剧本」语义更名「生成分镜」（分镜=分镜脚本；POST /script 为
// storyboard/generate 兼容别名，同一执行体）；顶栏按钮旁挂共用
// StoryboardGenPanel（出场人物/声线/镜头数/时长提示——配置随请求 body 发出，
// 后端 v1 未消费字段见 FilmStoryboardGenOptions 注释）。
const scriptBusy = ref(false);
const scriptError = ref('');
/** 生成分镜配置面板（工作台顶栏按钮共用；config() 取拼装结果）。 */
const wbGenPanel = ref<InstanceType<typeof StoryboardGenPanel> | null>(null);
const genBusy = reactive({ image: false, video: false, tts: false });
const shotError = ref('');
const composeBusy = ref(false);
const composeError = ref('');

// —— 工作台合成区（v0.1.36 纯总装）：BGM 选择（音频页库同源 filmListBgm，
//    trigger=global 缺省；「管理 BGM → 音频页」跳转）+ dist 成片版本迷你列表 ——
const bgmEntries = ref<FilmBgmEntry[]>([]);
const bgmTrack = ref('');
const bgmError = ref('');
const wbTree = ref<FilmFileEntry[]>([]);
/** 最新成片版本（dist/final-v*.mp4 新者在前；空=尚无版本）。 */
const latestVersion = computed(() => distVersions(wbTree.value)[0] ?? null);

async function loadWbBgm(): Promise<void> {
  const cur = project.value;
  if (!cur) return;
  try {
    const raw = await filmListBgm(cur.id);
    bgmEntries.value = Array.isArray(raw) ? raw : [];
    // 缺省：global 条目；否则第一项；空库=''
    if (bgmTrack.value && bgmEntries.value.some((e) => bgmEntryKey(e) === bgmTrack.value)) {
      // 既有选择仍有效
    } else {
      const global = bgmEntries.value.find(
        (e) => bgmEntryTrigger(e).toLowerCase() === 'global',
      );
      bgmTrack.value = global
        ? bgmEntryKey(global)
        : bgmEntryKey(bgmEntries.value[0] ?? {}) || '';
    }
    bgmError.value = '';
  } catch (e) {
    bgmEntries.value = [];
    bgmError.value = errMsg(e);
  }
}

async function loadWbTree(): Promise<void> {
  const cur = project.value;
  if (!cur) return;
  try {
    // v0.1.39.1 归一：{root, files} 信封 → 条目数组（裸数组/undefined 直通防御）
    const raw = await filmListFiles(cur.id);
    wbTree.value = hubFileEntries(raw);
  } catch {
    wbTree.value = []; // 版本列表派生面——静默降级（合成页有完整态）
  }
}

/** BGM 下拉选项尾注（非 global 显示 · mood）。 */
function wbBgmSuffix(e: FilmBgmEntry): string {
  const m = bgmEntryMood(e);
  return m ? ` · ${m}` : '';
}

/** 版本时间短格式（ISO → 本地；解析失败原样）。 */
function fmtWbTime(iso?: string | null): string {
  if (!iso) return '—';
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString();
}

/** 统一错误文案（film.rs 未就绪时 404/405 给出友好口径）。 */
function errMsg(e: unknown): string {
  const m = e instanceof Error ? e.message : String(e);
  if (/404|405|not found|method not allowed/i.test(m)) {
    return `${m}（film 后端可能尚未就绪）`;
  }
  return m;
}

/** 生成类动作公共骨架：校验 model_ref（「项目默认」=不传字段走缺省链）→
 *  提交 → 任务进任务中心（轮询）。 */
async function submitGen(
  cap: Cap,
  errRef: { value: string },
  run: (ref: FilmModelRef | undefined) => Promise<FilmTask>,
): Promise<void> {
  const useDefault = isProjectDefaultSel(cap);
  const ref = modelRefFor(cap);
  if (!useDefault && !ref) {
    errRef.value = hasOptionsFor(cap) ? t('film.pickModel') : t('film.noSource');
    return;
  }
  errRef.value = '';
  try {
    const task = await run(ref ?? undefined);
    addTracked(task);
  } catch (e) {
    errRef.value = t('film.actFailed') + errMsg(e);
    // v0.1.11 提交失败全局可见（此前仅局部红条——切走页面即不可见）
    toast.error(t('film.actFailed') + errMsg(e));
  }
}

async function genStoryboard(): Promise<void> {
  if (!project.value || scriptBusy.value) return;
  if (shots.value.length && !window.confirm(t('film.genStoryboardHint'))) return;
  scriptBusy.value = true;
  try {
    await submitGen('chat', scriptError, (ref) =>
      filmGenScript(project.value!.id, ref, wbGenPanel.value?.config()),
    );
  } finally {
    scriptBusy.value = false;
  }
}

async function genImage(): Promise<void> {
  if (!project.value || !selShot.value || genBusy.image) return;
  genBusy.image = true;
  try {
    await submitGen('image', shotError, (ref) =>
      filmGenShotImage(
        project.value!.id,
        selectedShot.value,
        ref,
        draft.image_prompt.trim() || undefined,
      ),
    );
  } finally {
    genBusy.image = false;
  }
}

async function genVideo(): Promise<void> {
  if (!project.value || !selShot.value || genBusy.video) return;
  genBusy.video = true;
  try {
    await submitGen('video', shotError, (ref) =>
      filmGenShotVideo(
        project.value!.id,
        selectedShot.value,
        ref,
        draft.video_prompt.trim() || undefined,
      ),
    );
  } finally {
    genBusy.video = false;
  }
}

async function genTts(): Promise<void> {
  if (!project.value || !selShot.value || genBusy.tts) return;
  genBusy.tts = true;
  try {
    await submitGen('tts', shotError, (ref) =>
      filmGenShotTts(
        project.value!.id,
        selectedShot.value,
        ref,
        draft.line.trim() || undefined,
      ),
    );
  } finally {
    genBusy.tts = false;
  }
}

async function composeFinal(): Promise<void> {
  const cur = project.value;
  if (!cur || composeBusy.value) return;
  composeBusy.value = true;
  composeError.value = '';
  try {
    const task = await filmCompose(cur.id, bgmTrack.value || undefined, flowAuthor.value);
    addTracked(task);
  } catch (e) {
    composeError.value = t('film.actFailed') + errMsg(e);
    toast.error(t('film.actFailed') + errMsg(e));
  } finally {
    composeBusy.value = false;
  }
}

/** 预览指定成片版本（监视器 final 模式装载该版本文件）。 */
function previewDistVersion(e: FilmFileEntry): void {
  previewEngine.setFinalName(e.path);
  if (previewEngine.mode.value !== 'final') previewEngine.setMode('final');
}

/** 下载指定成片版本（files b64 信封 → data URL → 浏览器另存）。 */
async function downloadDistVersion(e: FilmFileEntry): Promise<void> {
  const cur = project.value;
  if (!cur) return;
  try {
    const url = await filmFileDataUrl(cur.id, e.path);
    const a = document.createElement('a');
    a.href = url;
    a.download = fileBasename(e.path);
    a.click();
  } catch (err) {
    composeError.value = t('film.actFailed') + errMsg(err);
    toast.error(t('film.actFailed') + errMsg(err));
  }
}

// —— 导出路径设置（v0.1.34 契约：PUT /projects/:id 可选 export_dir，空串=重置为
//    项目目录；详情/PUT 回 export_dir + final_path，缺省 <项目目录>/final.mp4。
//    后端并行开发中：400 校验信息红条直显，旧后端 404/405 走 errMsg 友好口径）——
const showSettings = ref(false);
const settingsSaving = ref(false);
const settingsError = ref('');
const exportDirForm = ref('');

/** 当前成片落盘路径（后端 final_path 优先；旧后端无该字段时前端按缺省推导）。 */
const finalPath = computed(
  () =>
    project.value?.final_path ||
    (project.value?.dir ? `${project.value.dir.replace(/\/$/, '')}/final.mp4` : ''),
);

function openSettings(): void {
  exportDirForm.value = project.value?.export_dir ?? '';
  settingsError.value = '';
  showSettings.value = true;
}

async function saveExportDir(): Promise<void> {
  const cur = project.value;
  if (!cur || settingsSaving.value) return;
  // v0.1.39 前端预检（缓存目录审计）：非空时先本地校验绝对路径形态——
  // 后端校验（validate_export_dir：绝对路径 + 父目录须已存在不自动创建 +
  // 可写探针）兜底，400 文案直显；预检把最常见的「相对路径 / ~/ 开头」
  // 在前端就拦下（后端不自动 mkdir——缺目录须先手动创建）。
  const input = exportDirForm.value.trim();
  if (input && (!input.startsWith('/') || input.startsWith('~/'))) {
    settingsError.value = t('film.exportDirPrecheck');
    return;
  }
  settingsSaving.value = true;
  settingsError.value = '';
  try {
    const fresh = await filmUpdateProject(cur.id, { export_dir: input });
    // 保守合并：响应回显 export_dir/final_path（旧后端缺字段保留旧值）
    project.value = {
      ...cur,
      ...fresh,
      script: fresh.script ?? cur.script,
      artifacts: fresh.artifacts ?? cur.artifacts,
    };
    showSettings.value = false;
    // v0.1.11 保存类：导出路径保存成功全局反馈（此前仅弹窗静默关闭）
    toast.success(t('toast.saved'));
  } catch (e) {
    // 400 等：红条直显后端校验信息（不做前缀拼接，保留原始校验文案）
    settingsError.value = errMsg(e);
    toast.error(errMsg(e));
  } finally {
    settingsSaving.value = false;
  }
}

// =============================================================================
// 任务中心（FilmTask 轮询：2s，完成刷新项目产物）
// =============================================================================

function startPolling(): void {
  if (pollTimer !== null) return;
  pollTimer = setInterval(() => void pollTasks(), POLL_MS);
}

function stopPolling(): void {
  if (pollTimer !== null) {
    clearInterval(pollTimer);
    pollTimer = null;
  }
}

async function pollTasks(): Promise<void> {
  const active = trackedTasks.value.filter((x) => !x.done);
  if (active.length === 0) {
    stopPolling();
    return;
  }
  let finished = false;
  for (const item of active) {
    try {
      const task = await filmGetTask(item.id);
      // 后端字段映射：stage→kind；done/error→completed/failed（前端统一口径）
      item.kind = (task as unknown as { stage?: string }).stage ?? task.kind;
      item.status = task.status === 'done' ? 'completed' : task.status === 'error' ? 'failed' : task.status;
      const log = task.log ?? [];
      if (log.length) item.logTail = [...log];
      item.lastLog = log.length ? log[log.length - 1] : item.lastLog;
      if (item.createdAt === null) item.createdAt = taskCreatedAt(task.created_at);
      item.error = task.error ?? '';
      item.output = task.output ?? item.output;
      if (item.status === 'completed' || item.status === 'failed') {
        item.done = true;
        finished = true;
        // v0.1.11 闭环末环：终态通知（loading → success[产出摘要]/error[首行
        // 原因 + 查看任务钮]；toastId 失效时 notifier 内部重落一条不丢终态）
        taskNotifier.terminal(item.toastId ?? 0, {
          id: item.id,
          stage: item.kind,
          shot: item.shot,
          status: item.status,
          logTail: item.logTail,
          output: item.output,
          error: item.error,
        });
      } else if (item.toastId !== null) {
        // v0.1.11 过程可见：日志带「块 X/Y」→ loading toast 进度文案实时刷新
        // （embed 分钟级重任务的主反馈面；无分块进度静默不动）
        taskNotifier.progress(item.toastId, item.kind, item.shot, log);
      }
    } catch {
      // 单次轮询失败（瞬时网络/后端重启）：保留条目，下轮再试
    }
  }
  // 任一任务到终态 → 刷新项目（产物清单/refs 落到项目态）与角色（定妆图），
  // 并推进流程态（refreshTick → 流程页 watch 重载；README 阶段 + 协作流水）
  if (finished && mode.value === 'studio') {
    await reloadProject();
    void loadCharacters();
    void loadCastActions();
    void loadStage();
    void refreshCollab();
    refreshTick.value++;
  }
  if (trackedTasks.value.every((x) => x.done)) stopPolling();
}

/** 任务中心条目标签（kind + 关联镜头；白名单与 stage→i18n 键在
 *  flowContext.ts（FILM_TASK_STAGES / filmTaskStageLabel——含 v0.1.6 管线
 *  stage story 系 + casting；storyboard=分镜生成任务 stage，与 script
 *  （/script 兼容别名）同义）。 */
function taskLabel(item: TrackedTask): string {
  const kindText = filmTaskStageLabel(item.kind || '', t);
  return item.shot ? `${kindText}${t('film.taskShot', { n: item.shot })}` : kindText;
}

function taskStatusLabel(s: string): string {
  if (s === 'completed') return t('film.taskDone');
  if (s === 'failed') return t('film.taskFailed');
  if (s === 'running') return t('film.taskRunning');
  return t('film.taskQueued');
}

function dismissTask(id: string): void {
  trackedTasks.value = trackedTasks.value.filter((x) => x.id !== id);
}

// —— 一键提 Issue（v0.1.10：任务中心失败任务行 → from-task 自动成文；
//    成功 toast + 跳协作页——环节错误归因闭环的可视落点；v0.1.11 起 toast
//    走全局 NxToast 系统，不再用组件内固定浮层）——
/** from-task 提交中标记（任务 id；防重复点击）。 */
const issueBusy = ref('');

/** 失败任务 → Issue（读任务 stage/error/日志尾/源文件自动成文，后端落
 *  title=[环节] <stage> 失败 + labels=[pipeline-error] + stage 归因字段）。 */
async function fileIssueFromTask(item: TrackedTask): Promise<void> {
  const pid = project.value?.id;
  if (!pid || issueBusy.value) return;
  issueBusy.value = item.id;
  try {
    const res = await filmIssueFromTask(pid, {
      task_id: item.id,
      author: flowAuthor.value,
    });
    toast.success(t('collab.fromTaskDone', { n: res.issue?.iid ?? 0 }));
    setFlowView('collab');
  } catch (e) {
    toast.error(t('collab.fromTaskFailed') + errMsg(e));
  } finally {
    issueBusy.value = '';
  }
}

/** 离开工作室（顶栏「← 返回」/ SideNav 🎬 回大厅共用；v0.1.45 按来路回
 *  shotgen 短片生成或 list 大厅——任务轮询不中断，到终态自动停）。 */
function backToList(): void {
  mode.value = studioReturnMode.value;
}

// =============================================================================
// 角色库（2026-09-04 P0 一致性）+ 项目参考图
//
// 角色卡：名字/描述/voice 徽章/定妆图缩略/绑定镜头数；定妆图「上传」「生成」
// 双入口（生成带 model_ref 选择器，复用 image 能力面下拉）。镜头面板以 chips
// 增删绑定（PUT script.characters，后端按镜头号合并）；生成图/视频注入角色
// 参考（local=prompt 档 / channel=reference_images 档，语义差异见
// docs/FILM_STUDIO.md）；TTS 生效 voice = 绑定角色第一个 voice > 全局缺省。
// =============================================================================

const characters = ref<FilmCharacter[]>([]);
const charsError = ref('');
const charsLoading = ref(false);
/** 定妆图缩略（cid → data URL；经 files/download 信封懒加载）。 */
const charThumbs = reactive<Record<string, string>>({});

async function loadCharacters(): Promise<void> {
  const cur = project.value;
  if (!cur) return;
  charsLoading.value = true;
  charsError.value = '';
  try {
    characters.value = await filmListCharacters(cur.id);
    for (const c of characters.value) {
      if (c.portrait_url && !charThumbs[c.id]) void loadCharThumb(c.id, c.portrait_url);
    }
  } catch (e) {
    characters.value = [];
    charsError.value = t('film.charLoadFailed') + errMsg(e);
  } finally {
    charsLoading.value = false;
  }
}

// —— 动作类定妆对象（v0.1.41 P0：生成视频按钮旁「含动作参考/运镜」徽章）——
// 只读派生面：shot.actions 绑定 → 对象 motion/camera（后端生成链注入同源）。
const castActions = ref<FilmCastingObject[]>([]);

async function loadCastActions(): Promise<void> {
  const cur = project.value;
  if (!cur) return;
  try {
    const list = await filmListCasting(cur.id, 'actions');
    castActions.value = Array.isArray(list) ? list : [];
  } catch {
    castActions.value = []; // 旧后端 404 降级：徽章静默不显
  }
}

async function loadCharThumb(cid: string, url: string): Promise<void> {
  try {
    charThumbs[cid] = await fetchFileDataUrl(url);
  } catch {
    /* 缩略加载失败保持无图（上传/生成后有 portrait_url 再试） */
  }
}

/** 绑定镜头数（后端回传绑定清单；旧后端无此字段则不显）。 */
function boundCount(c: FilmCharacter): number {
  return c.bound_shots?.length ?? 0;
}

// —— 新建/编辑角色（三段式钉底弹窗）——
const showCharModal = ref(false);
const charEditing = ref<FilmCharacter | null>(null);
const charSaving = ref(false);
const charError = ref('');
const charForm = reactive({
  name: '',
  description: '',
  /** voice 取值形态：enum=OpenAI 11 枚举 / custom=自定义（渠道 voice_id 等）。 */
  voiceKind: 'enum' as 'enum' | 'custom' | 'none',
  voiceEnum: 'alloy',
  voiceCustom: '',
});

function openCreateChar(): void {
  charEditing.value = null;
  charForm.name = '';
  charForm.description = '';
  charForm.voiceKind = 'enum';
  charForm.voiceEnum = 'alloy';
  charForm.voiceCustom = '';
  charError.value = '';
  showCharModal.value = true;
}

function openEditChar(c: FilmCharacter): void {
  charEditing.value = c;
  charForm.name = c.name;
  charForm.description = c.description;
  if (!c.voice) {
    charForm.voiceKind = 'none';
    charForm.voiceEnum = 'alloy';
    charForm.voiceCustom = '';
  } else if ((OPENAI_VOICES as readonly string[]).includes(c.voice)) {
    charForm.voiceKind = 'enum';
    charForm.voiceEnum = c.voice;
    charForm.voiceCustom = '';
  } else {
    charForm.voiceKind = 'custom';
    charForm.voiceEnum = 'alloy';
    charForm.voiceCustom = c.voice;
  }
  charError.value = '';
  showCharModal.value = true;
}

function resolveVoiceBody(): string | undefined {
  if (charForm.voiceKind === 'custom') {
    return charForm.voiceCustom.trim() || undefined;
  }
  if (charForm.voiceKind === 'enum') return charForm.voiceEnum;
  return undefined;
}

async function submitChar(): Promise<void> {
  charError.value = '';
  if (!charForm.name.trim()) {
    charError.value = t('film.charErrName');
    return;
  }
  if (!charForm.description.trim()) {
    charError.value = t('film.charErrDesc');
    return;
  }
  charSaving.value = true;
  try {
    if (charEditing.value) {
      await filmUpdateCharacter(charEditing.value.id, {
        name: charForm.name.trim(),
        description: charForm.description.trim(),
        voice: resolveVoiceBody() ?? '',
      });
    } else {
      const voice = resolveVoiceBody();
      await filmCreateCharacter(project.value!.id, {
        name: charForm.name.trim(),
        description: charForm.description.trim(),
        ...(voice ? { voice } : {}),
      });
    }
    showCharModal.value = false;
    await loadCharacters();
    // v0.1.11 保存类：建/编角色成功全局反馈（此前仅弹窗关闭 + 列表刷新）
    toast.success(t('toast.saved'));
  } catch (e) {
    charError.value = t('film.charSaveFailed') + errMsg(e);
    toast.error(t('film.charSaveFailed') + errMsg(e));
  } finally {
    charSaving.value = false;
  }
}

async function removeChar(c: FilmCharacter): Promise<void> {
  if (!window.confirm(t('film.charDelConfirm', { name: c.name }))) return;
  try {
    await filmDeleteCharacter(c.id);
    delete charThumbs[c.id];
    await loadCharacters();
    // v0.1.11 危险确认类：删除成功 toast（失败从 window.alert 改全局 toast）
    toast.success(t('toast.deleted'));
  } catch (e) {
    toast.error(t('film.charDelFailed') + errMsg(e));
  }
}

// —— 定妆图上传（b64；≤10MB png/jpeg/webp）——
const portraitInput = ref<HTMLInputElement | null>(null);
const uploadingCharId = ref('');

function pickPortrait(cid: string): void {
  uploadingCharId.value = cid;
  portraitInput.value?.click();
}

async function onPortraitFile(e: Event): Promise<void> {
  const input = e.target as HTMLInputElement;
  const file = input.files?.[0];
  input.value = '';
  const cid = uploadingCharId.value;
  if (!file || !cid || !project.value) return;
  if (file.size > 10 * 1024 * 1024) {
    charsError.value = t('film.charTooLarge');
    return;
  }
  charsError.value = '';
  try {
    const { b64, mime } = splitDataUrl(await readFileAsDataUrl(file));
    const fresh = await filmUploadPortrait(project.value.id, cid, b64, mime || undefined);
    if (fresh.portrait_url) await loadCharThumb(cid, fresh.portrait_url);
    await loadCharacters();
    // v0.1.11 保存类：定妆图上传成败全局反馈
    toast.success(t('toast.saved'));
  } catch (err) {
    charsError.value = t('film.charUploadFailed') + errMsg(err);
    toast.error(t('film.charUploadFailed') + errMsg(err));
  }
}

// —— 定妆图生成（走既有生图面；model_ref 选择器复用 image 能力面下拉）——
const portraitModelSel = ref('');
const generatingCharId = ref('');

async function generatePortrait(c: FilmCharacter): Promise<void> {
  const cur = project.value;
  if (!cur) return;
  // 复用 image 能力面选择器：缺省沿用镜头面板当前选择
  const key = portraitModelSel.value || modelSel.image;
  if (!key) {
    charsError.value = t('film.pickModel');
    return;
  }
  const channelId = key.startsWith('ch:') ? key.slice(3) : undefined;
  const mr: FilmModelRef = channelId
    ? { source: 'channel', channel_id: channelId, capability: 'image' }
    : { source: 'local', capability: 'image' };
  charsError.value = '';
  generatingCharId.value = c.id;
  try {
    const task = await filmGenPortrait(cur.id, c.id, mr);
    addTracked(task);
  } catch (e) {
    charsError.value = t('film.actFailed') + errMsg(e);
    toast.error(t('film.actFailed') + errMsg(e));
  } finally {
    generatingCharId.value = '';
  }
}

// —— 镜头绑定编辑（chips 增删 → PUT script.characters）——
const bindBusy = ref(false);

/** 当前镜头出场角色名（旧后端无 characters 字段则视为空）。 */
const selShotCharacters = computed<string[]>(() => selShot.value?.characters ?? []);

/** 未绑定的角色（可添加项）。 */
const unboundCharacters = computed<FilmCharacter[]>(() =>
  characters.value.filter((c) => !selShotCharacters.value.includes(c.name)),
);

/** 参考注入数（图/视频：绑定角色中有定妆图的数量）。 */
const refInjectCount = computed(() =>
  characters.value.filter(
    (c) => selShotCharacters.value.includes(c.name) && !!c.portrait_ref,
  ).length,
);

/** 动作/运镜注入徽章（v0.1.41 P0：视频按钮旁「含动作参考 / 运镜：预设」）——
 *  当前镜头绑定动作对象的 motion/camera（与后端 resolve_action_injection 同
 *  语义：首个携带配置者）。 */
const motionBadge = computed<{ motion: boolean; camera: string } | null>(() => {
  for (const name of selShot.value?.actions ?? []) {
    const o = castActions.value.find((x) => x.name === name);
    if (!o) continue;
    const hasMotion = !!o.motion?.asset;
    const camera = o.camera?.kind === 'enum' ? o.camera.preset ?? '' : '';
    if (hasMotion || camera) return { motion: hasMotion, camera };
  }
  return null;
});

/** TTS 生效 voice：绑定角色第一个有 voice 的 > 全局缺省（env/alloy 由后端定）。 */
const effectiveVoice = computed<string | null>(() => {
  for (const name of selShotCharacters.value) {
    const c = characters.value.find((x) => x.name === name);
    if (c?.voice) return c.voice;
  }
  return null;
});

async function toggleShotCharacter(name: string, add: boolean): Promise<void> {
  const cur = project.value;
  const n = selectedShot.value;
  if (!cur || bindBusy.value) return;
  const next = add
    ? [...selShotCharacters.value, name]
    : selShotCharacters.value.filter((x) => x !== name);
  bindBusy.value = true;
  saveMsg.value = '';
  try {
    const fresh = await filmUpdateProject(cur.id, {
      script: [{ shot: n, characters: next }],
    });
    if (fresh.script) project.value = { ...cur, script: fresh.script };
    draftDirty.value = false;
    // v0.1.11 保存类：角色绑定/解绑成败全局反馈（此前仅失败有局部文案）
    toast.success(t('toast.saved'));
  } catch (e) {
    saveMsg.value = t('film.saveFailed') + errMsg(e);
    toast.error(t('film.saveFailed') + errMsg(e));
  } finally {
    bindBusy.value = false;
  }
}

/** 绑定下拉选中 → 添加绑定并复位下拉。 */
async function onAddBind(e: Event): Promise<void> {
  const sel = e.target as HTMLSelectElement;
  const name = sel.value;
  if (!name) return;
  sel.value = '';
  await toggleShotCharacter(name, true);
}

// —— 项目参考图导入（场景/风格参考；P0 仅管理）——
const refsInput = ref<HTMLInputElement | null>(null);
const refUploading = ref(false);
/** 参考图缩略（文件名 → data URL）。 */
const refThumbs = reactive<Record<string, string>>({});

function loadRefThumb(name: string): void {
  const cur = project.value;
  if (!cur?.dir || refThumbs[name]) return;
  void fetchFileDataUrl(`${cur.dir.replace(/\/$/, '')}/refs/${name}`)
    .then((u) => {
      refThumbs[name] = u;
    })
    .catch(() => {
      /* 缩略失败静默 */
    });
}

async function onRefsFile(e: Event): Promise<void> {
  const input = e.target as HTMLInputElement;
  const file = input.files?.[0];
  input.value = '';
  if (!file || !project.value) return;
  if (file.size > 10 * 1024 * 1024) {
    charsError.value = t('film.charTooLarge');
    return;
  }
  refUploading.value = true;
  charsError.value = '';
  try {
    const { b64 } = splitDataUrl(await readFileAsDataUrl(file));
    await filmUploadRef(project.value.id, b64, file.name);
    const fresh = await filmGetProject(project.value.id);
    project.value = { ...project.value, refs: fresh.refs ?? [] };
    for (const r of fresh.refs ?? []) loadRefThumb(r.name);
    // v0.1.11 保存类：参考图导入成败全局反馈
    toast.success(t('toast.imported', { name: file.name }));
  } catch (err) {
    charsError.value = t('film.refUploadFailed') + errMsg(err);
    toast.error(t('film.refUploadFailed') + errMsg(err));
  } finally {
    refUploading.value = false;
  }
}

// —— 产物预览（film 产物经 files/download 信封读取；图懒加载 / 视频·音频显状态）——
const shotPng = ref('');

watch([selectedShot, project], () => {
  const cur = project.value;
  shotPng.value = '';
  if (cur?.dir && hasArtifact(`shot-${selectedShot.value}.png`)) {
    void fetchFileDataUrl(
      `${cur.dir.replace(/\/$/, '')}/shot-${selectedShot.value}.png`,
    )
      .then((u) => {
        shotPng.value = u;
      })
      .catch(() => {
        shotPng.value = '';
      });
  }
  for (const r of cur?.refs ?? []) loadRefThumb(r.name);
});

/** 下载成片（files/download 信封 → Blob → 浏览器另存；大文件视内存而定）。 */
async function downloadFinal(): Promise<void> {
  const cur = project.value;
  if (!cur?.dir) return;
  try {
    const dataUrl = await fetchFileDataUrl(`${cur.dir.replace(/\/$/, '')}/final.mp4`);
    const a = document.createElement('a');
    a.href = dataUrl;
    a.download = `${cur.title || 'film'}-final.mp4`;
    a.click();
  } catch (e) {
    composeError.value = t('film.actFailed') + errMsg(e);
    toast.error(t('film.actFailed') + errMsg(e));
  }
}

// =============================================================================
// 生命周期
// =============================================================================
// 工作台合成区数据（BGM 库 + dist 成片版本）：进项目加载；任务终态刷新
watch(
  () => project.value?.id,
  (id) => {
    bgmTrack.value = '';
    bgmEntries.value = [];
    wbTree.value = [];
    if (id) {
      void loadWbBgm();
      void loadWbTree();
    }
  },
);
watch(refreshTick, () => {
  if (project.value) {
    void loadWbBgm();
    void loadWbTree();
  }
});

onMounted(() => {
  void initCaps(); // 能力快照先行（独立模式顶栏徽章数据源）
  void loadModelSources();
  // v0.1.45 短片生成首屏：作品流首拉（服务端历史；有进行中作品自动恢复轮询）
  shotgenEngine.start();
  // 深链（v0.1.5）：有 p → 列表就绪后校验项目并直达工作室视图（p 优先于
  // view=shotgen 首屏深链）；无 p → 默认首屏=短片生成（v0.1.45 产品变更）
  if (bootDeepLink.projectId) void loadProjects().then(applyBootDeepLink);
  else void loadProjects();
});

onUnmounted(() => {
  stopPolling();
  shotgenEngine.dispose();
  previewEngine.dispose();
  unsubCaps?.();
  unsubCaps = null;
});

// 模型源加载完成前用户已打开下拉的兜底：srcError 出现/清除时补默认项
watch(srcError, () => pickDefaultModels());
</script>

<template>
  <div class="film-page nx-page">
    <!-- ==================== 短片生成首屏（v0.1.45 默认；高级模式=下两大堂入口） ==================== -->
    <template v-if="mode === 'shotgen'">
      <ShotGen
        :video-ready="sgVideoReady"
        :model-name="sgModelName"
        :is-offline="isOffline"
        @advanced="goShotgenAdvanced"
        @go-models="goShotgenModels"
      />
    </template>

    <!-- ==================== FilmHub 大厅（高级模式；v0.1.1 显性大厅；HubLobby 组件） ==================== -->
    <template v-else-if="mode === 'list'">
      <div v-if="srcError" class="warn-box">{{ t('film.srcFailed') }}{{ srcError }}</div>
      <HubLobby
        :projects="projects"
        :loading="listLoading"
        :error="listError"
        @refresh="loadProjects"
        @create="openCreate"
        @open="enterStudio"
        @browse="browseProject"
        @delete="removeProject"
      >
        <!-- 宿主侧杂项（能力徽章 / 独立模式外链）——大厅顶栏尾部 -->
        <template #head-extra>
          <!-- 全局任务指示器（v0.1.11：返回大厅后任务照跑照可见） -->
          <TaskIndicator :tasks="indicatorTasks" @view-all="goWorkbenchTasks" />
          <!-- 能力徽章（独立模式；全能力=无徽章） -->
          <span
            v-if="isStandalone && deg && deg.mode !== 'full'"
            class="caps-badge"
            :class="deg.mode === 'offline' ? 'caps-off' : 'caps-deg'"
            :title="deg.mode === 'offline'
              ? t('film.capsOfflineTip')
              : t('film.capsMissingTip', { caps: deg.missing.join(', ') })"
          >{{ deg.mode === 'offline' ? t('film.capsOffline') : t('film.capsDegraded') }}</span>
          <button
            v-if="!isStandalone"
            class="btn btn-small btn-ext"
            type="button"
            :title="t('film.openStandalone')"
            :aria-label="t('film.openStandalone')"
            @click="openStandalone"
          >
            <svg class="ext-icon" viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
              <path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" />
              <polyline points="15 3 21 3 21 9" />
              <line x1="10" y1="14" x2="21" y2="3" />
            </svg>
          </button>
        </template>
      </HubLobby>
    </template>

    <!-- ==================== 项目工作室 ==================== -->
    <template v-else>
      <!-- 顶栏：返回 + 标题/比例 + 生成剧本（chat 模型选择器） -->
      <div class="studio-top">
        <button class="btn btn-small" @click="backToList">
          ← {{ t('film.back') }}
        </button>
        <div class="studio-meta">
          <span class="studio-title" :title="project?.title">{{ project?.title }}</span>
          <!-- 比例 pill：显示预设名（landscape→手机横版）；title 附比例与输出分辨率 -->
          <span
            class="pill pill-ratio"
            :title="presetRes(project?.ratio)
              ? `${project?.ratio ?? ''} · ${presetRes(project?.ratio)}`
              : (project?.ratio ?? '')"
          >{{ presetName(project?.ratio) || project?.ratio }}</span>
          <span
            v-if="project?.style_hint"
            class="pill pill-muted"
            :title="project.style_hint"
          >{{ project.style_hint }}</span>
          <!-- ⚙ 导出路径设置（导出目录 → final_path；v0.1.34 契约） -->
          <button
            class="btn btn-small btn-gear"
            type="button"
            :title="t('film.exportDirTitle')"
            :aria-label="t('film.exportDirSettings')"
            :disabled="!project"
            @click="openSettings"
          >⚙</button>
        </div>
        <div class="studio-gen">
          <!-- 全局任务指示器（v0.1.11：↻ N 徽章 + popover 明细——全部页面可见） -->
          <TaskIndicator :tasks="indicatorTasks" @view-all="goWorkbenchTasks" />
          <!-- 深浅主题切换（v0.1.8 铺满批次：工作室顶栏与流程页/大厅同款） -->
          <NxThemeToggle />
          <!-- 能力徽章（独立模式；全能力=无徽章） -->
          <span
            v-if="isStandalone && deg && deg.mode !== 'full'"
            class="caps-badge"
            :class="deg.mode === 'offline' ? 'caps-off' : 'caps-deg'"
            :title="deg.mode === 'offline'
              ? t('film.capsOfflineTip')
              : t('film.capsMissingTip', { caps: deg.missing.join(', ') })"
          >{{ deg.mode === 'offline' ? t('film.capsOffline') : t('film.capsDegraded') }}</span>
          <!-- 「我是」操作人（写操作 author 字段；localStorage 记忆，设置页可改） -->
          <select
            class="msel-select"
            :value="flowAuthor"
            :title="t('film.whoAmITip')"
            @change="onAuthorChange"
          >
            <option value="anonymous">👤 anonymous</option>
            <option v-for="m in memberOptions" :key="m" :value="m">👤 {{ m }}</option>
          </select>
          <!-- 成本徽章（调用数+估算费用；点击弹只读面板 by stage/channel） -->
          <CostBadge />
          <!-- 生成分镜（工作台视图内；流程化后分镜生成移至分镜页；v0.1.39
               文案「生成剧本」→「生成分镜」——分镜=分镜脚本，POST /script 为
               storyboard/generate 兼容别名） -->
          <template v-if="navView === 'workbench'">
            <select v-model="modelSel.chat" class="msel-select" :title="t('film.model')">
              <!-- 🏷 项目默认（v0.1.38）：不传 model_ref 字段，走 models.json 缺省链 -->
              <option :value="PROJECT_DEFAULT_KEY">
                🏷 {{ t('models.projectDefault') }}{{ defaultModelSummary('chat') ? ' · ' + defaultModelSummary('chat') : '' }}
              </option>
              <option v-if="!hasOptionsFor('chat')" value="" disabled>
                {{ t('film.noRunningLlm') }}
              </option>
              <optgroup
                v-for="g in optionsFor('chat')"
                :key="g.label"
                :label="g.label"
              >
                <option v-for="o in g.options" :key="o.key" :value="o.key">
                  {{ o.label }}{{ o.relay ? ' 🌐' : '' }}
                </option>
              </optgroup>
            </select>
            <button
              class="btn btn-primary btn-small"
              :disabled="scriptBusy || !project || !modelSelReady('chat') || !chatAvailable"
              :title="genDisabledTip('script') ?? t('film.genStoryboardHint')"
              @click="genStoryboard"
            >
              <span v-if="scriptBusy" class="spin spinning" aria-hidden="true">↻</span>
              {{ scriptBusy ? t('film.btnBusy') : t('film.genStoryboard') }}
            </button>
          </template>
          <button
            v-if="!isStandalone"
            class="btn btn-small btn-ext"
            type="button"
            :title="t('film.openStandalone')"
            :aria-label="t('film.openStandalone')"
            @click="openStandalone"
          >
            <svg class="ext-icon" viewBox="0 0 24 24" width="14" height="14" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
              <path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" />
              <polyline points="15 3 21 3 21 9" />
              <line x1="10" y1="14" x2="21" y2="3" />
            </svg>
          </button>
        </div>
      </div>
      <div v-if="scriptError" class="error-box">{{ scriptError }}</div>
      <!-- 生成分镜配置面板（v0.1.39 共用组件：出场人物 chips + 声线下拉 +
           镜头数 + 总时长提示；配置随顶栏「生成分镜」请求 body 发出） -->
      <StoryboardGenPanel v-if="navView === 'workbench'" ref="wbGenPanel" />
      <div v-if="projectError" class="error-box">
        {{ projectError }}
        <button class="btn btn-small" @click="reloadProject">{{ t('film.retry') }}</button>
      </div>

      <!-- 主体（v0.1.35 FilmHub 流程化）：左侧选项卡栏（SideNav：五阶段 + 工作台
           + 设置/成员；阶段徽章读 README stage）+ 页面主体（流程页各自独立组件，
           FlowContext 共享会话态；「工作台」= 原五区） -->
      <div class="studio-shell">
        <SideNav :view="navView" :stage="flowStage" @select="setFlowView" @home="backToList" @file-click="onNavTreeFileClick" />
        <div class="studio-content">

          <!-- ==================== 工作台视图（原五区，全保留） ==================== -->
          <template v-if="navView === 'workbench'">
      <!-- 五区主体（v0.1.35）：左镜头卡纵列 24% + 紧凑任务条 / 中镜头面板 40%
           （角色区在面板列内）/ 右预览监视器 36% + 紧凑合成区（窄窗纵向堆叠，
           监视器置顶；底部另接多轨时间轴条——见 .studio-main 之后） -->
      <div class="studio-main">
        <!-- ① 左：镜头时间轴纵列（条状卡，容器滚动）+ 底部紧凑任务条 -->
        <section class="col col-timeline">
          <div class="card col-shots">
            <div class="col-head">
              <span>{{ t('film.timeline') }}</span>
              <span class="muted small">{{ shots.length }}</span>
            </div>
            <div class="strip">
              <div v-if="shots.length === 0" class="strip-empty muted">
                {{ projectLoading ? t('film.loading') : t('film.timelineEmpty') }}
              </div>
              <button
                v-for="s in shots"
                :key="s.shot"
                class="shot-card"
                :class="{ active: s.shot === selectedShot }"
                type="button"
                @click="selectedShot = s.shot"
              >
                <span class="shot-no mono">#{{ s.shot }}</span>
                <span class="shot-state" :title="shotState(s).label">
                  {{ shotState(s).icon }} {{ shotState(s).label }}
                </span>
                <span class="shot-desc" :title="s.desc ?? ''">
                  {{ s.desc || '—' }}
                </span>
                <span class="shot-arts">
                  <span class="art-dot" :class="{ on: hasArtifact(`shot-${s.shot}.png`) }">🖼</span>
                  <span class="art-dot" :class="{ on: hasArtifact(`shot-${s.shot}.mp4`) }">▶</span>
                  <span class="art-dot" :class="{ on: hasArtifact(`line-${s.shot}.mp3`) }">🔊</span>
                </span>
              </button>
            </div>
          </div>

          <!-- 紧凑任务条（任务中心从右栏迁来）：收起=一行摘要，展开=列表+日志尾 -->
          <div class="card taskbar" :class="{ open: tasksOpen }">
            <button
              class="taskbar-head"
              type="button"
              :aria-expanded="tasksOpen"
              @click="tasksOpen = !tasksOpen"
            >
              <span
                class="spin"
                :class="{ spinning: activeTaskCount > 0 }"
                aria-hidden="true"
              >↻</span>
              <span class="taskbar-title">
                {{ activeTaskCount
                  ? t('film.tasksBarRunning', { n: activeTaskCount })
                  : t('film.tasksBarIdle') }}
              </span>
              <span class="nx-badge nx-badge--sm nx-badge--neutral taskbar-count mono">{{ trackedTasks.length }}</span>
              <span class="taskbar-caret" aria-hidden="true">{{ tasksOpen ? '▾' : '▴' }}</span>
            </button>
            <div v-if="tasksOpen" class="taskbar-list">
              <div v-if="trackedTasks.length === 0" class="strip-empty muted">
                {{ t('film.tasksEmpty') }}
              </div>
              <div
                v-for="item in trackedTasks"
                :key="item.id"
                class="task-item"
                :class="{ 'is-failed': item.status === 'failed' }"
              >
                <div class="task-row1">
                  <span class="task-kind">{{ taskLabel(item) }}</span>
                  <span
                    class="nx-badge nx-badge--sm"
                    :class="item.status === 'completed'
                      ? 'nx-badge--success'
                      : item.status === 'failed'
                        ? 'nx-badge--danger'
                        : 'nx-badge--info'"
                  >
                    <span
                      v-if="item.status === 'running' || item.status === 'queued'"
                      class="nx-badge__dot"
                      :class="{ 'nx-badge__dot--pulse': item.status === 'running' }"
                      aria-hidden="true"
                    />{{ taskStatusLabel(item.status) }}
                  </span>
                  <!-- 失败任务一键提 Issue（from-task：环节错误自动成文 → 协作页） -->
                  <button
                    v-if="item.status === 'failed'"
                    class="task-issue-btn"
                    type="button"
                    :title="t('collab.fromTaskTip')"
                    :disabled="issueBusy === item.id"
                    @click="fileIssueFromTask(item)"
                  >🐞 {{ issueBusy === item.id ? '…' : t('collab.fromTask') }}</button>
                  <button
                    v-if="item.done"
                    class="task-dismiss"
                    type="button"
                    :title="t('film.cancel')"
                    @click="dismissTask(item.id)"
                  >×</button>
                </div>
                <div v-if="item.lastLog" class="task-log mono" :title="item.lastLog">
                  {{ item.lastLog }}
                </div>
                <div v-if="item.error" class="task-err">{{ item.error }}</div>
              </div>
            </div>
          </div>
        </section>

        <!-- ② 中：选中镜头面板（编辑 + 生成按钮 + 产物预览）+ 角色库（面板列内） -->
        <section class="col col-shot">
          <section ref="shotPanelEl" class="card shot-panel">
          <div class="col-head">
            <span>{{ t('film.shotPanel') }}</span>
            <span v-if="selShot" class="mono">#{{ selectedShot }}</span>
          </div>
          <div class="col-body">
            <div v-if="!selShot" class="strip-empty muted">
              {{ shots.length ? t('film.noShot') : t('film.timelineEmpty') }}
            </div>
            <template v-else>
              <label class="field">
                <span class="field-label">{{ t('film.fDesc') }}</span>
                <textarea
                  v-model="draft.description"
                  rows="2"
                  :disabled="savingShot"
                  @input="markDirty"
                />
              </label>
              <label class="field">
                <span class="field-label">{{ t('film.fImgPrompt') }}</span>
                <textarea
                  v-model="draft.image_prompt"
                  rows="2"
                  :disabled="savingShot"
                  @input="markDirty"
                />
              </label>
              <label class="field">
                <span class="field-label">{{ t('film.fVidPrompt') }}</span>
                <textarea
                  v-model="draft.video_prompt"
                  rows="2"
                  :disabled="savingShot"
                  @input="markDirty"
                />
              </label>
              <div class="field-row">
                <label class="field field-grow">
                  <span class="field-label">{{ t('film.fLine') }}</span>
                  <textarea
                    v-model="draft.line"
                    rows="2"
                    :disabled="savingShot"
                    @input="markDirty"
                  />
                </label>
                <label class="field field-narrow">
                  <span class="field-label">{{ t('film.fDuration') }}</span>
                  <input
                    v-model.number="draft.duration_secs"
                    type="number"
                    min="0"
                    step="0.5"
                    :disabled="savingShot"
                    @input="markDirty"
                  />
                </label>
              </div>
              <div class="save-row">
                <button
                  class="btn btn-small"
                  :disabled="savingShot || !draftDirty"
                  @click="saveShot"
                >
                  {{ savingShot ? t('film.saving') : t('film.saveShot') }}
                </button>
                <span v-if="saveMsg" class="save-msg" :class="{ err: saveMsg.startsWith(t('film.saveFailed')) }">
                  {{ saveMsg }}
                </span>
              </div>

              <!-- 出场角色绑定（chips 增删 → PUT script.characters） -->
              <div class="field">
                <span class="field-label">{{ t('film.charBind') }}</span>
                <div class="chip-row">
                  <span v-for="name in selShotCharacters" :key="name" class="chip">
                    {{ name }}
                    <button
                      class="chip-x"
                      type="button"
                      :disabled="bindBusy"
                      :title="t('film.charUnbind')"
                      @click="toggleShotCharacter(name, false)"
                    >×</button>
                  </span>
                  <select
                    v-if="unboundCharacters.length"
                    class="chip-add"
                    :disabled="bindBusy"
                    :title="t('film.charAddBind')"
                    @change="onAddBind($event)"
                  >
                    <option value="" disabled>+ {{ t('film.charAddBind') }}</option>
                    <option v-for="c in unboundCharacters" :key="c.id" :value="c.name">
                      {{ c.name }}
                    </option>
                  </select>
                  <span v-if="!characters.length" class="muted small">{{ t('film.charNoneHint') }}</span>
                </div>
              </div>

              <!-- 生成动作：图（本地 sd-turbo + 渠道）/ 视频 / 配音（仅渠道）。
                   各选择器首项 🏷「项目默认」（v0.1.38）= 不传 model_ref，走
                   models.json 缺省链；选项文案内联当前默认源摘要 -->
              <div v-if="shotError" class="error-box error-box-slim">{{ shotError }}</div>
              <div class="gen-row">
                <div class="gen-cell">
                  <select v-model="modelSel.image" class="msel-select" :title="t('film.model')">
                    <option :value="PROJECT_DEFAULT_KEY">
                      🏷 {{ t('models.projectDefault') }}{{ defaultModelSummary('image') ? ' · ' + defaultModelSummary('image') : '' }}
                    </option>
                    <option v-if="!hasOptionsFor('image')" value="" disabled>
                      {{ t('film.noSource') }}
                    </option>
                    <optgroup v-for="g in optionsFor('image')" :key="g.label" :label="g.label">
                      <option v-for="o in g.options" :key="o.key" :value="o.key">
                        {{ o.label }}{{ o.relay ? ' 🌐' : '' }}
                      </option>
                    </optgroup>
                  </select>
                  <button
                    class="btn btn-small"
                    :disabled="genBusy.image || !modelSelReady('image') || isOffline"
                    :title="isOffline ? t('film.capsOfflineTip') : undefined"
                    @click="genImage"
                  >
                    {{ genBusy.image ? '…' : t('film.genImage') }}
                  </button>
                  <span v-if="refInjectCount" class="gen-hint" :title="t('film.refInjectTip')">
                    🧩 {{ t('film.refInject', { n: refInjectCount }) }}
                  </span>
                </div>
                <div class="gen-cell">
                  <select v-model="modelSel.video" class="msel-select" :title="t('film.model')">
                    <option :value="PROJECT_DEFAULT_KEY">
                      🏷 {{ t('models.projectDefault') }}{{ defaultModelSummary('video') ? ' · ' + defaultModelSummary('video') : '' }}
                    </option>
                    <option v-if="!hasOptionsFor('video')" value="" disabled>
                      {{ t('film.channelOnlyHint') }}
                    </option>
                    <optgroup v-for="g in optionsFor('video')" :key="g.label" :label="g.label">
                      <option v-for="o in g.options" :key="o.key" :value="o.key">
                        {{ o.label }}{{ o.relay ? ' 🌐' : '' }}
                      </option>
                    </optgroup>
                  </select>
                  <button
                    class="btn btn-small"
                    :disabled="genBusy.video || !modelSelReady('video') || !channelAvailable"
                    :title="genDisabledTip('media') ?? (modelSelReady('video') ? '' : t('film.channelOnlyHint'))"
                    @click="genVideo"
                  >
                    {{ genBusy.video ? '…' : t('film.genVideo') }}
                  </button>
                  <span v-if="refInjectCount" class="gen-hint" :title="t('film.refInjectTip')">
                    🧩 {{ t('film.refInject', { n: refInjectCount }) }}
                  </span>
                  <!-- v0.1.41 P0 动作/运镜注入徽章（分镜绑定动作对象的 motion/camera） -->
                  <span
                    v-if="motionBadge"
                    class="gen-hint"
                    :title="t('film.motionBadgeTip')"
                  >
                    <template v-if="motionBadge.motion">🎞 {{ t('film.motionRefBadge') }}</template>
                    <template v-if="motionBadge.camera">🎥 {{ t('film.cameraBadge', { preset: motionBadge.camera }) }}</template>
                  </span>
                </div>
                <div class="gen-cell">
                  <select v-model="modelSel.tts" class="msel-select" :title="t('film.model')">
                    <option :value="PROJECT_DEFAULT_KEY">
                      🏷 {{ t('models.projectDefault') }}{{ defaultModelSummary('tts') ? ' · ' + defaultModelSummary('tts') : '' }}
                    </option>
                    <option v-if="!hasOptionsFor('tts')" value="" disabled>
                      {{ t('film.channelOnlyHint') }}
                    </option>
                    <optgroup v-for="g in optionsFor('tts')" :key="g.label" :label="g.label">
                      <option v-for="o in g.options" :key="o.key" :value="o.key">
                        {{ o.label }}{{ o.relay ? ' 🌐' : '' }}
                      </option>
                    </optgroup>
                  </select>
                  <button
                    class="btn btn-small"
                    :disabled="genBusy.tts || !modelSelReady('tts') || !channelAvailable"
                    :title="genDisabledTip('media') ?? (modelSelReady('tts') ? '' : t('film.channelOnlyHint'))"
                    @click="genTts"
                  >
                    {{ genBusy.tts ? '…' : t('film.genTts') }}
                  </button>
                  <span class="gen-hint" :title="t('film.voiceTip')">
                    🎙 {{ effectiveVoice ? t('film.voiceActive', { voice: effectiveVoice }) : t('film.voiceDefault') }}
                  </span>
                </div>
              </div>

              <!-- 产物预览（图经 files/download 信封取字节；视频/音频显产物状态） -->
              <div class="arts">
                <div class="art-block">
                  <div class="art-title">🖼 {{ t('film.artImage') }}</div>
                  <img
                    v-if="shotPng"
                    class="art-img"
                    :src="shotPng"
                    :alt="t('film.artImage')"
                  />
                  <div v-else class="art-none muted">{{ t('film.artNone') }}</div>
                </div>
                <div class="art-block">
                  <div class="art-title">▶ {{ t('film.artVideo') }}</div>
                  <div v-if="hasArtifact(`shot-${selectedShot}.mp4`)" class="art-ready">
                    ✅ shot-{{ selectedShot }}.mp4
                  </div>
                  <div v-else class="art-none muted">{{ t('film.artNone') }}</div>
                </div>
                <div class="art-block">
                  <div class="art-title">🔊 {{ t('film.artAudio') }}</div>
                  <div v-if="hasArtifact(`line-${selectedShot}.mp3`)" class="art-ready">
                    ✅ line-{{ selectedShot }}.mp3
                  </div>
                  <div v-else class="art-none muted">{{ t('film.artNone') }}</div>
                </div>
              </div>
            </template>
          </div>
          </section>

          <!-- 角色库（定妆图 / voice / 绑定镜头数；参考图导入入口；
               v0.1.35 从右栏并入中栏——右栏让位预览监视器） -->
          <div class="card side-block side-chars">
            <div class="col-head">
              <span>{{ t('film.charTitle') }}</span>
              <span class="muted small">{{ characters.length }}</span>
              <button class="btn btn-small head-btn" :disabled="!project" @click="openCreateChar">
                + {{ t('film.charNew') }}
              </button>
            </div>
            <div class="char-toolbar">
              <select v-model="portraitModelSel" class="msel-select" :title="t('film.charPortraitModel')">
                <option value="" disabled>{{ t('film.charPortraitModel') }}</option>
                <optgroup v-for="g in optionsFor('image')" :key="g.label" :label="g.label">
                  <option v-for="o in g.options" :key="o.key" :value="o.key">
                    {{ o.label }}{{ o.relay ? ' 🌐' : '' }}
                  </option>
                </optgroup>
              </select>
              <button
                class="btn btn-small"
                :disabled="refUploading || !project || isOffline"
                :title="isOffline ? t('film.capsOfflineTip') : undefined"
                @click="refsInput?.click()"
              >
                {{ refUploading ? '…' : t('film.refImport') }}
              </button>
              <input ref="refsInput" type="file" accept="image/png,image/jpeg,image/webp" class="hidden-input" @change="onRefsFile" />
            </div>
            <div v-if="charsError" class="error-box error-box-slim">{{ charsError }}</div>
            <div class="char-list">
              <div v-if="!characters.length && !charsLoading" class="strip-empty muted">
                {{ t('film.charEmpty') }}
              </div>
              <div v-for="c in characters" :key="c.id" class="char-card">
                <div class="char-thumb">
                  <img v-if="charThumbs[c.id]" :src="charThumbs[c.id]" :alt="c.name" />
                  <span v-else class="char-thumb-ph">👤</span>
                </div>
                <div class="char-main">
                  <div class="char-row1">
                    <span class="char-name" :title="c.name">{{ c.name }}</span>
                    <span v-if="c.voice" class="pill pill-voice mono" :title="t('film.charVoiceTip')">🎙 {{ c.voice }}</span>
                    <span v-if="c.portrait_ref" class="pill pill-ok">{{ t('film.charPortraitOk') }}</span>
                  </div>
                  <div class="char-desc muted" :title="c.description">{{ c.description }}</div>
                  <div class="char-meta muted small">
                    {{ t('film.charBound', { n: boundCount(c) }) }}
                  </div>
                </div>
                <div class="char-actions">
                  <button class="btn btn-small" :disabled="uploadingCharId === c.id" @click="pickPortrait(c.id)">
                    {{ uploadingCharId === c.id ? '…' : t('film.charUpload') }}
                  </button>
                  <button
                    class="btn btn-small"
                    :disabled="generatingCharId === c.id || !(portraitModelSel || modelSel.image) || isOffline"
                    :title="t('film.charGenTip')"
                    @click="generatePortrait(c)"
                  >
                    {{ generatingCharId === c.id ? '…' : t('film.charGen') }}
                  </button>
                  <button class="btn btn-small" @click="openEditChar(c)">{{ t('film.charEdit') }}</button>
                  <button class="btn btn-small btn-danger" @click="removeChar(c)">{{ t('film.del') }}</button>
                </div>
              </div>
            </div>
            <!-- 参考图（场景/风格参考；P0 仅管理） -->
            <div v-if="(project?.refs ?? []).length" class="ref-strip">
              <div class="art-title">{{ t('film.refTitle') }}</div>
              <div class="ref-grid">
                <div v-for="r in project?.refs ?? []" :key="r.name" class="ref-item" :title="`${r.name} · ${r.bytes}B`">
                  <img v-if="refThumbs[r.name]" :src="refThumbs[r.name]" :alt="r.name" />
                  <span v-else class="char-thumb-ph">🖼</span>
                </div>
              </div>
            </div>
            <input ref="portraitInput" type="file" accept="image/png,image/jpeg,image/webp" class="hidden-input" @change="onPortraitFile" />
          </div>
        </section>

        <!-- ③ 右：预览监视器（主体）+ 下方紧凑合成区（BGM 一行 + 合成/导出） -->
        <section class="col col-preview">
          <!-- 预览监视器：previewEngine 驱动（⏮⏭ 联动选中镜头） -->
          <PreviewMonitor @select-shot="onTimelineSelect" />

          <!-- 紧凑合成区（监视器下；v0.1.36 纯总装形态：无 AI 生成入口——
               BGM 从音频页库选择，素材生成归流程页） -->
          <div class="card side-block compose-compact">
            <div class="col-head">{{ t('film.composeArea') }}</div>
            <div class="side-body">
              <label class="field">
                <span class="field-label">
                  🎵 {{ t('film.wbBgmSelect') }}
                  <button
                    class="wb-link-btn"
                    type="button"
                    :title="t('film.wbBgmManage')"
                    @click="setFlowView('audio')"
                  >{{ t('film.wbBgmManage') }} →</button>
                </span>
                <select
                  v-model="bgmTrack"
                  :disabled="!bgmEntries.length"
                  :title="bgmEntries.length ? t('film.wbBgmSelect') : t('film.wbBgmEmpty')"
                >
                  <option value="">{{ t('film.cpBgmDefault') }}</option>
                  <option v-for="e in bgmEntries" :key="bgmEntryKey(e)" :value="bgmEntryKey(e)">
                    {{ bgmEntryTrigger(e) || bgmEntryKey(e) }}{{ wbBgmSuffix(e) }}
                  </option>
                </select>
              </label>
              <div v-if="bgmError" class="error-box error-box-slim">{{ t('film.bgmLoadFailed') }}{{ bgmError }}</div>

              <div class="compose-row">
                <button
                  class="btn btn-primary btn-small compose-btn"
                  :disabled="composeBusy || !project || !composeAvailable"
                  :title="genDisabledTip('compose')"
                  @click="composeFinal"
                >
                  {{ composeBusy ? t('film.taskRunning') + '…' : t('film.compose') }}
                </button>
                <!-- 最新成片版本：预览/下载（完整版本列表在合成页） -->
                <template v-if="latestVersion">
                  <button
                    class="btn btn-small"
                    :class="{ 'btn-final-on': previewingFinal }"
                    :title="latestVersion.path"
                    @click="previewDistVersion(latestVersion)"
                  >▶ {{ t('film.cpPreview') }}</button>
                  <button
                    class="btn btn-small"
                    :title="latestVersion.path"
                    @click="downloadDistVersion(latestVersion)"
                  >⬇ {{ t('film.cpDownload') }}</button>
                </template>
                <!-- 旧项目遗留 final.mp4（无 dist 版本时的兜底） -->
                <template v-else-if="hasArtifact('final.mp4')">
                  <button
                    class="btn btn-small"
                    :class="{ 'btn-final-on': previewingFinal }"
                    :title="hasArtifact('final.mp4') ? t('film.finalTitle') : t('film.finalEmpty')"
                    @click="toggleFinalPreview"
                  >🎬 {{ previewingFinal ? t('film.previewStoryboard') : t('film.previewFinal') }}</button>
                  <button
                    class="btn btn-small"
                    @click="downloadFinal"
                  >⬇ {{ t('film.download') }}</button>
                </template>
              </div>
              <!-- 成片版本迷你列表（最新一版 + 完整版入口） -->
              <div v-if="latestVersion" class="wb-version-mini">
                <span class="pill pill-blue pill-mini">v{{ distVersionOf(latestVersion.path) }}</span>
                <span class="muted small wb-version-meta">
                  {{ fmtWbTime(entryMtime(latestVersion)) }} · {{ fmtBytes(latestVersion.bytes) }}
                </span>
                <button
                  class="wb-link-btn"
                  type="button"
                  :title="t('film.wbAllVersions')"
                  @click="setFlowView('compose')"
                >{{ t('film.wbAllVersions') }} →</button>
              </div>
              <!-- 导出路径小字（final_path；⚙ 弹窗可改导出目录） -->
              <span class="export-path muted" :title="finalPath">
                {{ t('film.exportAt', { path: finalPath || '—' }) }}
              </span>
              <div v-if="composeError" class="error-box error-box-slim">{{ composeError }}</div>
            </div>
          </div>
        </section>
      </div>

      <!-- ④ 底部多轨时间轴条（TimelineTracks：视频/配音/BGM/字幕四轨，可折叠
           182px→28px；块点击=选中镜头——联动左侧镜头卡 + 滚动镜头面板到选中；
           块状态由 shots/artifacts props 派生，任务轮询终态 reloadProject 后自动
           变色；缩放/标尺/未尽事项见组件头注释） -->
      <TimelineTracks
        :shots="shots"
        :artifacts="project?.artifacts ?? []"
        :selected-shot="selectedShot"
        :project-dir="project?.dir ?? ''"
        @select="onTimelineSelect"
      />
          </template>

          <!-- ==================== 流程页（各自独立组件；FlowContext 注入） ==================== -->
          <StoryPage v-else-if="navView === 'story'" />
          <StoryboardPage v-else-if="navView === 'storyboard'" />
          <CastingPage v-else-if="navView === 'casting'" />
          <AudioPage v-else-if="navView === 'audio'" />
          <ComposePage v-else-if="navView === 'compose'" />
          <ModelsPage v-else-if="navView === 'models'" />
          <CollabPage v-else-if="navView === 'collab'" />
          <HubBrowse v-else-if="navView === 'hub'" />
          <SettingsPage v-else />
        </div>
      </div>
    </template>

    <!-- ==================== 新建项目对话框（三段式钉底弹窗） ==================== -->
    <div
      v-if="showCreate"
      class="modal-backdrop"
      @click.self="showCreate = false"
    >
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="film-new-title">
        <div class="modal-head">
          <h3 id="film-new-title">{{ t('film.newTitle') }}</h3>
          <button class="modal-close" type="button" @click="showCreate = false">×</button>
        </div>
        <div class="modal-body">
          <label class="field">
            <span class="field-label">{{ t('film.fTitle') }} *</span>
            <input
              v-model="createForm.title"
              type="text"
              :placeholder="t('film.fTitlePh')"
              :disabled="creating"
            />
          </label>
          <label class="field">
            <span class="field-label">{{ t('film.fIdea') }} *</span>
            <textarea
              v-model="createForm.idea"
              rows="4"
              :placeholder="t('film.fIdeaPh')"
              :disabled="creating"
            />
          </label>
          <div class="field">
            <span class="field-label">{{ t('film.fRatio') }}</span>
            <!-- 六档预设卡网格（2 行 ×3 列）：图示比例条 + 名称 + 分辨率 + 用途；
                 选中高亮；hover title 提示全说明（名称 · 比例 · 分辨率 — 用途） -->
            <div class="ratio-grid" role="radiogroup" :aria-label="t('film.fRatio')">
              <button
                v-for="p in RATIO_PRESETS"
                :key="p.key"
                type="button"
                class="ratio-card"
                :class="{ sel: createForm.ratio === p.ratio }"
                role="radio"
                :aria-checked="createForm.ratio === p.ratio"
                :disabled="creating"
                :title="`${t(`film.preset${presetKeyCap(p.key)}`)}（${p.ratio} · ${p.width}×${p.height}）— ${t(`film.preset${presetKeyCap(p.key)}Desc`)}`"
                @click="createForm.ratio = p.ratio"
              >
                <span class="ratio-bar-box"><span class="ratio-bar" :style="presetBarStyle(p)"></span></span>
                <span class="ratio-name">{{ presetName(p.ratio) }}</span>
                <span class="ratio-res mono">{{ p.width }}×{{ p.height }}</span>
                <span class="ratio-use">{{ presetDesc(p.ratio) }}</span>
              </button>
            </div>
          </div>
          <label class="field">
            <span class="field-label">{{ t('film.fStyle') }}</span>
            <input
              v-model="createForm.style_hint"
              type="text"
              :placeholder="t('film.fStylePh')"
              :disabled="creating"
            />
          </label>
          <div v-if="createError" class="error-box">{{ createError }}</div>
          <div class="form-actions">
            <button class="btn" type="button" :disabled="creating" @click="showCreate = false">
              {{ t('film.cancel') }}
            </button>
            <button class="btn btn-primary" type="button" :disabled="creating" @click="submitCreate">
              {{ creating ? t('film.creating') : t('film.create') }}
            </button>
          </div>
        </div>
      </div>
    </div>

    <!-- ==================== 角色新建/编辑对话框（三段式钉底弹窗） ==================== -->
    <div
      v-if="showCharModal"
      class="modal-backdrop"
      @click.self="showCharModal = false"
    >
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="film-char-title">
        <div class="modal-head">
          <h3 id="film-char-title">
            {{ charEditing ? t('film.charEditTitle', { name: charEditing.name }) : t('film.charNewTitle') }}
          </h3>
          <button class="modal-close" type="button" @click="showCharModal = false">×</button>
        </div>
        <div class="modal-body">
          <label class="field">
            <span class="field-label">{{ t('film.charName') }} *</span>
            <input
              v-model="charForm.name"
              type="text"
              :placeholder="t('film.charNamePh')"
              :disabled="charSaving"
            />
          </label>
          <label class="field">
            <span class="field-label">{{ t('film.charDesc') }} *</span>
            <textarea
              v-model="charForm.description"
              rows="3"
              :placeholder="t('film.charDescPh')"
              :disabled="charSaving"
            />
            <span class="muted small">{{ t('film.charDescHint') }}</span>
          </label>
          <div class="field">
            <span class="field-label">{{ t('film.charVoice') }}</span>
            <div class="field-row">
              <select v-model="charForm.voiceKind" class="msel-select" :disabled="charSaving">
                <option value="enum">{{ t('film.charVoiceEnum') }}</option>
                <option value="custom">{{ t('film.charVoiceCustom') }}</option>
                <option value="none">{{ t('film.charVoiceNone') }}</option>
              </select>
              <select v-if="charForm.voiceKind === 'enum'" v-model="charForm.voiceEnum" class="msel-select" :disabled="charSaving">
                <option v-for="v in OPENAI_VOICES" :key="v" :value="v">{{ v }}</option>
              </select>
              <input
                v-if="charForm.voiceKind === 'custom'"
                v-model="charForm.voiceCustom"
                type="text"
                :placeholder="t('film.charVoiceCustomPh')"
                :disabled="charSaving"
                class="field-input"
              />
            </div>
            <span class="muted small">{{ t('film.charVoiceHint') }}</span>
          </div>
          <div v-if="charError" class="error-box">{{ charError }}</div>
          <div class="form-actions">
            <button class="btn" type="button" :disabled="charSaving" @click="showCharModal = false">
              {{ t('film.cancel') }}
            </button>
            <button class="btn btn-primary" type="button" :disabled="charSaving" @click="submitChar">
              {{ charSaving ? t('film.saving') : t('film.create') }}
            </button>
          </div>
        </div>
      </div>
    </div>
    <!-- ==================== 导出路径设置对话框（三段式钉底弹窗） ==================== -->
    <div
      v-if="showSettings"
      class="modal-backdrop"
      @click.self="showSettings = false"
    >
      <div class="modal" role="dialog" aria-modal="true" aria-labelledby="film-settings-title">
        <div class="modal-head">
          <h3 id="film-settings-title">{{ t('film.exportDirTitle') }}</h3>
          <button class="modal-close" type="button" @click="showSettings = false">×</button>
        </div>
        <div class="modal-body">
          <label class="field">
            <span class="field-label">{{ t('film.exportDirLabel') }}</span>
            <input
              v-model="exportDirForm"
              type="text"
              class="mono"
              :placeholder="finalPath"
              :disabled="settingsSaving"
              spellcheck="false"
            />
            <span class="muted small">{{ t('film.exportDirHint') }}</span>
          </label>
          <div class="field">
            <span class="field-label">{{ t('film.exportDirCurrent') }}</span>
            <div class="final-path-row mono small">{{ finalPath || '—' }}</div>
          </div>
          <div v-if="settingsError" class="error-box">{{ settingsError }}</div>
          <div class="form-actions">
            <button class="btn" type="button" :disabled="settingsSaving" @click="showSettings = false">
              {{ t('film.cancel') }}
            </button>
            <button
              class="btn btn-primary"
              type="button"
              :disabled="settingsSaving || !project"
              @click="saveExportDir"
            >
              {{ settingsSaving ? t('film.saving') : t('film.exportDirSave') }}
            </button>
          </div>
        </div>
      </div>
    </div>

    <!-- 全局通知渲染器（v0.1.11：toast 单例状态的右上角堆叠浮层；Teleport
         body——桌面嵌入与 standalone 两载体同享，挂根一次即可） -->
    <NxToast />
  </div>
</template>

<style scoped>
/* ===================== v0.1.8 UI 重设计第二批：scoped 全量迁 design token =====================
   类名/布局骨架不变；视觉（底色/描边/圆角/阴影/字色/焦点环）改用 --nx-* 变量，
   深浅主题随 <html data-theme> 换值。间距刻度维持 4px 系。 */
/* ===================== 页面骨架（零 vh：flex + min-height:0） ===================== */
.film-page {
  height: 100%;
  min-height: 420px;
  display: flex;
  flex-direction: column;
  gap: 12px;
  padding: 16px 18px;
  overflow: hidden;
}
.muted { color: var(--nx-text-tertiary); }
.small { font-size: var(--nx-font-size-xs); }
.mono { font-family: var(--nx-font-mono); }

.card {
  background: var(--nx-bg-card);
  border: 0.5px solid var(--nx-line-regular);
  border-radius: var(--nx-radius-card);
  box-shadow: var(--nx-shadow-xs);
}

/* pill 徽章（soft 六色 token 刻面 + badge 圆角档） */
.pill {
  display: inline-flex;
  align-items: center;
  padding: 2px 8px;
  border-radius: var(--nx-radius-badge);
  font-size: var(--nx-font-size-xs);
  border: 1px solid var(--nx-line-regular);
  background: var(--nx-bg-hover);
  color: var(--nx-text-secondary);
  line-height: 1.4;
  white-space: nowrap;
  flex-shrink: 0;
}
.pill-ok { color: var(--nx-success); background: var(--nx-success-bg); border-color: var(--nx-success-border); }
.pill-err { color: var(--nx-danger); background: var(--nx-danger-bg); border-color: var(--nx-danger-border); }
.pill-blue { color: var(--nx-info); background: var(--nx-info-bg); border-color: var(--nx-info-border); }
.pill-muted { color: var(--nx-text-tertiary); }
.pill-ratio {
  font-family: var(--nx-font-mono);
  font-size: var(--nx-font-size-xs);
  padding: 1px 8px;
  color: var(--nx-text-tertiary);
}

/* 能力徽章（@nexos/app-sdk 降级三态；全能力=不渲染；warning/danger soft） */
.caps-badge {
  display: inline-flex;
  align-items: center;
  padding: 2px 10px;
  border-radius: var(--nx-radius-badge);
  font-size: var(--nx-font-size-xs);
  white-space: nowrap;
  flex-shrink: 0;
  cursor: help;
}
.caps-deg {
  color: var(--nx-warning);
  background: var(--nx-warning-bg);
  border: 1px solid var(--nx-warning-border);
}
.caps-off {
  color: var(--nx-danger);
  background: var(--nx-danger-bg);
  border: 1px solid var(--nx-danger-border);
}

/* ===================== 工作室：顶栏（标题/操作人/成本徽章/刷新） ===================== */
.studio-top {
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
  flex-shrink: 0;
}
.studio-meta {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
  flex: 1;
}
.studio-title {
  font-size: var(--nx-font-size-md);
  font-weight: var(--nx-font-weight-semibold);
  color: var(--nx-text-primary);
  letter-spacing: -0.01em;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  max-width: 40%;
}
.studio-gen {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
  margin-left: auto;
}

/* ===================== 工作室：左侧选项卡 + 页面主体（v0.1.35 FilmHub） ===================== */
.studio-shell {
  flex: 1;
  min-height: 0;
  display: flex;
  gap: 12px;
}
.studio-content {
  flex: 1;
  min-width: 0;
  min-height: 0;
  display: flex;
  flex-direction: column;
}

/* ===================== 工作室：五区（左 24 / 中 40 / 右 36，窄窗堆叠监视器置顶） ===================== */
.studio-main {
  flex: 1;
  min-height: 0;
  display: flex;
  gap: 12px;
}
.col {
  display: flex;
  flex-direction: column;
  min-width: 0;
  min-height: 0;
}
.col-timeline { flex: 24 1 0; gap: 12px; }
.col-shot { flex: 40 1 0; gap: 12px; }
.col-preview { flex: 36 1 0; gap: 12px; }
/* 区头：卡片头刻面（divider 底线 + semibold + tertiary 计数） */
.col-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  padding: 10px 16px;
  border-bottom: 0.5px solid var(--nx-line-divider);
  font-size: var(--nx-font-size-base);
  font-weight: var(--nx-font-weight-semibold);
  color: var(--nx-text-primary);
  flex-shrink: 0;
}
.col-body {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: 12px 16px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}

/* ① 时间轴纵列：镜头卡卡体（card 包一层，flex 列让 strip 内滚） */
.col-shots {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  overflow: hidden;
}
/* ① 紧凑任务条（任务中心；收起=聚合徽章行，展开=列表+日志尾） */
.taskbar {
  flex-shrink: 0;
  display: flex;
  flex-direction: column;
  overflow: hidden;
}
.taskbar-head {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  padding: 6px 12px;
  border: none;
  background: transparent;
  font-family: inherit;
  font-size: var(--nx-font-size-sm);
  font-weight: var(--nx-font-weight-medium);
  color: var(--nx-text-secondary);
  cursor: pointer;
  text-align: left;
  transition: background 0.12s ease, color 0.12s ease;
}
.taskbar-head:hover { background: var(--nx-bg-hover); color: var(--nx-text-primary); }
.taskbar-head:focus-visible {
  outline: 2px solid var(--nx-ring-color);
  outline-offset: -2px;
}
.taskbar-title {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.taskbar-count { flex-shrink: 0; }
.taskbar-caret { color: var(--nx-text-tertiary); font-size: 11px; flex-shrink: 0; }
.taskbar-list {
  max-height: 240px;
  overflow-y: auto;
  padding: 8px 10px;
  border-top: 0.5px solid var(--nx-line-divider);
  display: flex;
  flex-direction: column;
  gap: 8px;
}

/* ① 时间轴：条状卡横排流（容器滚动） */
.strip {
  flex: 1;
  min-height: 0;
  overflow: auto;
  display: flex;
  flex-wrap: wrap;
  align-content: flex-start;
  gap: 8px;
  padding: 10px;
}
.strip-empty {
  padding: 18px 10px;
  font-size: var(--nx-font-size-sm);
  text-align: center;
  width: 100%;
}
/* 镜头卡三态：静默 0.5px 描边 / hover 浮起（换底+描边加深+阴影）/ 选中 accent+soft */
.shot-card {
  flex: 0 0 140px;
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding: 8px 10px;
  border: 0.5px solid var(--nx-line-regular);
  border-radius: var(--nx-radius-input);
  background: var(--nx-bg-card);
  cursor: pointer;
  text-align: left;
  font-family: inherit;
  font-size: var(--nx-font-size-xs);
  color: var(--nx-text-secondary);
  transition:
    border-color 0.15s ease,
    background 0.15s ease,
    box-shadow 0.15s ease;
}
.shot-card:hover {
  border-color: var(--nx-line-strong);
  background: var(--nx-bg-subtle);
  box-shadow: var(--nx-shadow-xs);
}
.shot-card.active {
  border-color: var(--nx-accent);
  background: var(--nx-accent-soft);
  color: var(--nx-text-primary);
  box-shadow: var(--nx-shadow-xs);
}
.shot-card:focus-visible {
  outline: 2px solid var(--nx-ring-color);
  outline-offset: 1px;
}
.shot-no {
  font-weight: var(--nx-font-weight-semibold);
  font-size: var(--nx-font-size-sm);
  color: var(--nx-text-primary);
}
.shot-state {
  font-size: var(--nx-font-size-xs);
  color: var(--nx-text-tertiary);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.shot-desc {
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
  line-height: 1.45;
  min-height: 2.9em;
}
.shot-arts { display: flex; gap: 6px; }
/* 产物状态点：未产 = 中性 30% / 已产 = success 点亮 */
.art-dot { opacity: 0.3; font-size: 11px; color: var(--nx-text-tertiary); }
.art-dot.on { opacity: 1; color: var(--nx-success); }

/* ② 镜头面板表单（nx-input 刻面 + focus 环三件套） */
.field {
  display: flex;
  flex-direction: column;
  gap: 4px;
  min-width: 0;
}
.field-grow { flex: 1; }
.field-narrow { width: 130px; flex-shrink: 0; }
.field-label {
  font-size: var(--nx-font-size-xs);
  font-weight: var(--nx-font-weight-medium);
  color: var(--nx-text-tertiary);
}
.field input,
.field textarea,
.field select {
  font-family: inherit;
  font-size: var(--nx-font-size-sm);
  padding: 6px 10px;
  border: 1px solid var(--nx-line-strong);
  border-radius: var(--nx-radius-input);
  background: var(--nx-bg-card);
  color: var(--nx-text-primary);
  resize: vertical;
  min-width: 0;
  transition:
    border-color 0.15s ease,
    box-shadow 0.15s ease;
}
.field input:focus-visible,
.field textarea:focus-visible,
.field select:focus-visible {
  outline: none;
  border-color: var(--nx-accent);
  box-shadow: 0 0 0 3px var(--nx-ring-color);
}
.field input:disabled,
.field textarea:disabled,
.field select:disabled {
  background: var(--nx-bg-subtle);
  color: var(--nx-text-disabled);
  cursor: not-allowed;
}
.field-row {
  display: flex;
  gap: 10px;
  align-items: flex-start;
  flex-wrap: wrap;
}

/* —— 分辨率预设卡网格（新建弹窗；2 行 ×3 列，六档；nx-card 单选刻面）—— */
.ratio-grid {
  display: grid;
  grid-template-columns: repeat(3, 1fr);
  gap: 8px;
}
.ratio-card {
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 3px;
  padding: 9px 6px 7px;
  border: 0.5px solid var(--nx-line-regular);
  border-radius: var(--nx-radius-input);
  background: var(--nx-bg-card);
  color: var(--nx-text-secondary);
  font-family: inherit;
  cursor: pointer;
  min-width: 0;
  text-align: center;
  transition:
    border-color 0.12s ease,
    box-shadow 0.12s ease,
    background 0.12s ease;
}
.ratio-card:hover:not(:disabled) {
  border-color: var(--nx-line-strong);
  background: var(--nx-bg-subtle);
  box-shadow: var(--nx-shadow-xs);
}
.ratio-card.sel {
  border-color: var(--nx-accent);
  box-shadow:
    0 0 0 1px var(--nx-accent) inset,
    var(--nx-shadow-xs);
  background: var(--nx-accent-soft);
  color: var(--nx-text-primary);
}
.ratio-card:disabled { opacity: 0.55; cursor: not-allowed; }
.ratio-card:focus-visible {
  outline: 2px solid var(--nx-ring-color);
  outline-offset: 1px;
}
/* 图示比例条容器（44×32 定高盒内居中画 mini 条，条按 presetBarStyle 比例） */
.ratio-bar-box {
  height: 32px;
  display: flex;
  align-items: center;
  justify-content: center;
}
.ratio-bar {
  display: inline-block;
  border-radius: 3px;
  background: var(--nx-text-tertiary);
  opacity: 0.55;
}
.ratio-card.sel .ratio-bar {
  background: var(--nx-accent);
  opacity: 1;
}
.ratio-name {
  font-size: var(--nx-font-size-xs);
  font-weight: var(--nx-font-weight-medium);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  max-width: 100%;
}
.ratio-res {
  font-family: var(--nx-font-mono);
  font-size: 11px;
  color: var(--nx-text-tertiary);
}
.ratio-use {
  font-size: 11px;
  line-height: 1.35;
  color: var(--nx-text-tertiary);
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
  min-height: 28px;
}
/* 窄弹窗（<560px 视口）：预设卡折两列 */
@media (max-width: 480px) {
  .ratio-grid { grid-template-columns: repeat(2, 1fr); }
}

.field-input {
  font-family: inherit;
  font-size: var(--nx-font-size-sm);
  padding: 5px 10px;
  border: 1px solid var(--nx-line-strong);
  border-radius: var(--nx-radius-input);
  background: var(--nx-bg-card);
  color: var(--nx-text-primary);
  min-width: 0;
  flex: 1;
}
.field-input:focus-visible {
  outline: none;
  border-color: var(--nx-accent);
  box-shadow: 0 0 0 3px var(--nx-ring-color);
}
.save-row {
  display: flex;
  align-items: center;
  gap: 10px;
}
.save-msg { font-size: var(--nx-font-size-xs); color: var(--nx-success); }
.save-msg.err { color: var(--nx-danger); }

/* 生成行：每格 = 模型选择器 + 按钮 */
.gen-row {
  display: flex;
  flex-direction: column;
  gap: 8px;
  border-top: 1px dashed var(--nx-line-regular);
  padding-top: 10px;
}
.gen-cell {
  display: flex;
  gap: 8px;
  align-items: center;
  min-width: 0;
}
.gen-cell .msel-select { flex: 1; min-width: 0; }
/* 模型选择器（nx-select 刻面 + focus 环） */
.msel-select {
  font-family: inherit;
  font-size: var(--nx-font-size-xs);
  padding: 5px 8px;
  border: 1px solid var(--nx-line-strong);
  border-radius: var(--nx-radius-input);
  background: var(--nx-bg-card);
  color: var(--nx-text-primary);
  cursor: pointer;
  max-width: 100%;
  transition:
    border-color 0.15s ease,
    box-shadow 0.15s ease;
}
.msel-select:focus-visible {
  outline: none;
  border-color: var(--nx-accent);
  box-shadow: 0 0 0 3px var(--nx-ring-color);
}
.msel-select:disabled {
  background: var(--nx-bg-subtle);
  color: var(--nx-text-disabled);
  cursor: not-allowed;
}

/* 产物预览 */
.arts {
  display: flex;
  flex-direction: column;
  gap: 10px;
  border-top: 1px dashed var(--nx-line-regular);
  padding-top: 10px;
}
.art-block {
  display: flex;
  flex-direction: column;
  gap: 6px;
  min-width: 0;
}
.art-title {
  font-size: var(--nx-font-size-xs);
  font-weight: var(--nx-font-weight-medium);
  color: var(--nx-text-tertiary);
}
.art-img {
  max-width: 100%;
  max-height: 220px;
  border-radius: var(--nx-radius-input);
  border: 0.5px solid var(--nx-line-regular);
  object-fit: contain;
  align-self: flex-start;
}
.art-video {
  width: 100%;
  max-height: 220px;
  border-radius: var(--nx-radius-input);
  background: #000000; /* 视频画面底：深浅主题恒黑（媒体语义色） */
}
.art-audio { width: 100%; height: 36px; }
.art-none {
  font-size: var(--nx-font-size-xs);
  color: var(--nx-text-tertiary);
  padding: 10px 12px;
  border: 1px dashed var(--nx-line-strong);
  border-radius: var(--nx-radius-input);
}

/* ② 镜头面板卡（中栏上卡；编辑 + 生成按钮 + 产物预览） */
.shot-panel {
  flex: 1 1 58%;
  min-height: 0;
  display: flex;
  flex-direction: column;
  overflow: hidden;
}

/* ③ 右栏：角色库（中栏下卡）+ 紧凑合成区 */
.side-block {
  display: flex;
  flex-direction: column;
  min-height: 0;
}
.side-chars { flex: 1 1 42%; }
.compose-compact { flex-shrink: 0; }
.head-btn { margin-left: auto; }

/* 紧凑合成区小徽章（bgm.mp3 就绪提示，嵌在 field-label 行内） */
.pill-mini {
  font-size: 10px;
  padding: 0 6px;
  margin-left: 4px;
  vertical-align: 1px;
}
/* 「预览成片」激活态（final 模式中 → accent soft 底示意可切回） */
.btn-final-on {
  background: var(--nx-accent-soft);
  border-color: var(--nx-accent);
  color: var(--nx-accent);
}

/* 角色库：工具行 + 角色卡列表 */
.char-toolbar {
  display: flex;
  gap: 8px;
  padding: 8px 10px;
  border-bottom: 0.5px solid var(--nx-line-divider);
  flex-shrink: 0;
}
.char-toolbar .msel-select { flex: 1; min-width: 0; }
.char-list {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: 8px 10px;
  display: flex;
  flex-direction: column;
  gap: 8px;
}
/* 角色卡：静默描边卡 + hover 描边加深 */
.char-card {
  border: 0.5px solid var(--nx-line-regular);
  border-radius: var(--nx-radius-input);
  background: var(--nx-bg-card);
  padding: 8px 10px;
  display: flex;
  gap: 10px;
  font-size: var(--nx-font-size-xs);
  color: var(--nx-text-secondary);
  transition: border-color 0.15s ease, box-shadow 0.15s ease;
}
.char-card:hover { border-color: var(--nx-line-strong); box-shadow: var(--nx-shadow-xs); }
.char-thumb {
  width: 48px;
  height: 48px;
  flex-shrink: 0;
  border-radius: var(--nx-radius-input);
  border: 0.5px solid var(--nx-line-regular);
  overflow: hidden;
  display: flex;
  align-items: center;
  justify-content: center;
  background: var(--nx-bg-subtle);
}
.char-thumb img { width: 100%; height: 100%; object-fit: cover; }
.char-thumb-ph { font-size: 22px; opacity: 0.5; }
.char-main { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 2px; }
.char-row1 { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
.char-name {
  font-weight: var(--nx-font-weight-semibold);
  font-size: var(--nx-font-size-sm);
  color: var(--nx-text-primary);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.pill-voice {
  font-family: var(--nx-font-mono);
  font-size: 11px;
  padding: 0 8px;
  color: var(--nx-accent);
  background: var(--nx-accent-soft);
  border-color: var(--nx-accent-border);
}
.char-desc {
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
  font-size: var(--nx-font-size-xs);
  line-height: 1.45;
}
.char-meta { font-size: 11px; }
.char-actions {
  display: flex;
  flex-direction: column;
  gap: 4px;
  flex-shrink: 0;
}
.char-actions .btn { padding: 2px 8px; font-size: 11px; }

/* 参考图缩略条 */
.ref-strip {
  border-top: 0.5px solid var(--nx-line-divider);
  padding: 8px 10px;
  display: flex;
  flex-direction: column;
  gap: 6px;
  flex-shrink: 0;
}
.ref-grid { display: flex; flex-wrap: wrap; gap: 6px; }
.ref-item {
  width: 40px;
  height: 40px;
  border-radius: var(--nx-radius-badge);
  border: 0.5px solid var(--nx-line-regular);
  overflow: hidden;
  display: flex;
  align-items: center;
  justify-content: center;
  background: var(--nx-bg-subtle);
}
.ref-item img { width: 100%; height: 100%; object-fit: cover; }
.hidden-input { display: none; }

/* 镜头面板：角色绑定 chips（info soft 刻面） */
.chip-row {
  display: flex;
  flex-wrap: wrap;
  gap: 6px;
  align-items: center;
  min-height: 28px;
}
.chip {
  display: inline-flex;
  align-items: center;
  gap: 2px;
  padding: 2px 4px 2px 10px;
  border-radius: var(--nx-radius-pill);
  font-size: var(--nx-font-size-xs);
  background: var(--nx-info-bg);
  border: 1px solid var(--nx-info-border);
  color: var(--nx-info);
}
.chip-x {
  background: transparent;
  border: none;
  font-size: 13px;
  line-height: 1;
  color: inherit;
  opacity: 0.7;
  cursor: pointer;
  padding: 0 4px;
}
.chip-x:hover { opacity: 1; color: var(--nx-danger); }
.chip-x:focus-visible { outline: 2px solid var(--nx-ring-color); outline-offset: 1px; border-radius: 4px; }
.chip-add {
  font-family: inherit;
  font-size: var(--nx-font-size-xs);
  padding: 3px 8px;
  border: 1px dashed var(--nx-line-strong);
  border-radius: var(--nx-radius-pill);
  background: var(--nx-bg-card);
  color: var(--nx-text-tertiary);
  cursor: pointer;
}
.chip-add:focus-visible { outline: 2px solid var(--nx-ring-color); outline-offset: 1px; }

/* 生成行注入提示 */
.gen-hint {
  font-size: 11px;
  color: var(--nx-text-tertiary);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  flex-shrink: 0;
}
/* 产物已生成状态行（success soft） */
.art-ready {
  font-size: var(--nx-font-size-xs);
  color: var(--nx-success);
  padding: 10px 12px;
  border: 1px dashed var(--nx-success-border);
  border-radius: var(--nx-radius-input);
  background: var(--nx-success-bg);
}

/* 任务条目（紧凑任务条展开列表；失败红 = danger soft 整行） */
.task-item {
  border: 0.5px solid var(--nx-line-regular);
  border-radius: var(--nx-radius-input);
  background: var(--nx-bg-subtle);
  padding: 8px 10px;
  display: flex;
  flex-direction: column;
  gap: 4px;
  font-size: var(--nx-font-size-xs);
  color: var(--nx-text-secondary);
}
.task-item.is-failed {
  border-color: var(--nx-danger-border);
  background: var(--nx-danger-bg);
}
.task-row1 {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
}
.task-kind {
  font-weight: var(--nx-font-weight-medium);
  color: var(--nx-text-primary);
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.task-dismiss {
  background: transparent;
  border: none;
  font-size: 16px;
  line-height: 1;
  color: var(--nx-text-tertiary);
  cursor: pointer;
  padding: 0 2px;
}
.task-dismiss:hover { color: var(--nx-text-primary); }
.task-dismiss:focus-visible { outline: 2px solid var(--nx-ring-color); outline-offset: 1px; border-radius: 4px; }
/* 一键提 Issue（失败任务行；v0.1.10——destructive soft 底与失败徽章同族） */
.task-issue-btn {
  margin-left: auto;
  padding: 1px 8px;
  font-size: var(--nx-font-size-xs);
  font-family: inherit;
  border-radius: var(--nx-radius-badge, 6px);
  border: 1px solid var(--nx-danger-border, var(--nx-line-regular));
  background: var(--nx-danger-bg, transparent);
  color: var(--nx-danger, inherit);
  cursor: pointer;
  line-height: 1.5;
  white-space: nowrap;
}
.task-issue-btn:hover:not(:disabled) { filter: brightness(1.05); }
.task-issue-btn:disabled { opacity: 0.55; cursor: default; }
.task-issue-btn:focus-visible { outline: 2px solid var(--nx-ring-color); outline-offset: 1px; }
.task-log {
  font-family: var(--nx-font-mono);
  font-size: 11px;
  color: var(--nx-text-tertiary);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.task-err { font-size: 11px; color: var(--nx-danger); word-break: break-all; }
.side-body {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: 12px 16px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.compose-row {
  display: flex;
  gap: 8px;
  align-items: center;
  flex-wrap: wrap;
  border-top: 1px dashed var(--nx-line-regular);
  padding-top: 10px;
}
.compose-btn { flex: 1; }
.art-final { max-height: 180px; }

/* 工作台纯总装区（v0.1.36）：「管理 BGM → 音频页」/「完整版本 → 合成页」链接钮 */
.wb-link-btn {
  border: none;
  background: transparent;
  color: var(--nx-accent);
  font-family: inherit;
  font-size: 11px;
  line-height: 1.4;
  padding: 0 2px;
  cursor: pointer;
  white-space: nowrap;
}
.wb-link-btn:hover { text-decoration: underline; }
.wb-link-btn:focus-visible { outline: 2px solid var(--nx-ring-color); outline-offset: 1px; border-radius: 4px; }

/* 成片版本迷你列表行（最新一版 + 完整版入口） */
.wb-version-mini {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
  min-width: 0;
}
.wb-version-meta {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}

/* 导出路径小字（合成成片按钮旁；final_path 可能较长→break-all 换行） */
.export-path {
  flex: 1 1 100%;
  font-size: 11px;
  line-height: 1.5;
  word-break: break-all;
  min-width: 0;
}
/* ⚙ 设置钮（顶栏标题旁；与 btn-small 同风格，仅字号略大衬齿轮形） */
.btn-gear { padding: 4px 9px; font-size: 13.5px; line-height: 1; flex-shrink: 0; }
/* 导出设置弹窗：当前成片路径展示行（subtle 内衬 + dashed） */
.final-path-row {
  font-family: var(--nx-font-mono);
  font-size: var(--nx-font-size-xs);
  padding: 8px 10px;
  border: 1px dashed var(--nx-line-strong);
  border-radius: var(--nx-radius-input);
  background: var(--nx-bg-subtle);
  color: var(--nx-text-secondary);
  word-break: break-all;
}

/* ===================== 消息条（nx-alert 四色刻面） ===================== */
.error-box {
  color: var(--nx-danger);
  background: var(--nx-danger-bg);
  border: 1px solid var(--nx-danger-border);
  padding: 10px 14px;
  border-radius: var(--nx-radius-input);
  font-size: var(--nx-font-size-sm);
  flex-shrink: 0;
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
  word-break: break-all;
}
.error-box-slim { padding: 6px 10px; font-size: var(--nx-font-size-xs); }
.warn-box {
  color: var(--nx-warning);
  background: var(--nx-warning-bg);
  border: 1px solid var(--nx-warning-border);
  padding: 8px 14px;
  border-radius: var(--nx-radius-input);
  font-size: var(--nx-font-size-xs);
  flex-shrink: 0;
}

/* ===================== 按钮（nx-btn secondary/primary/danger 刻面） ===================== */
.btn {
  padding: 6px 12px;
  border-radius: var(--nx-radius-input);
  border: 1px solid var(--nx-line-strong);
  background: var(--nx-bg-card);
  color: var(--nx-text-secondary);
  font-size: var(--nx-font-size-sm);
  font-weight: var(--nx-font-weight-medium);
  cursor: pointer;
  font-family: inherit;
  text-decoration: none;
  display: inline-flex;
  align-items: center;
  gap: 4px;
  justify-content: center;
  transition:
    background 0.15s ease,
    border-color 0.15s ease,
    color 0.15s ease,
    box-shadow 0.15s ease;
  flex-shrink: 0;
}
.btn:hover:not(:disabled) { background: var(--nx-bg-hover); color: var(--nx-text-primary); }
.btn:active:not(:disabled) { background: var(--nx-bg-burn); }
.btn:disabled {
  background: transparent;
  border-color: var(--nx-line-regular);
  color: var(--nx-text-disabled);
  cursor: not-allowed;
}
.btn:focus-visible {
  outline: 2px solid var(--nx-ring-color);
  outline-offset: var(--nx-ring-offset);
}
.btn-small { padding: 4px 10px; font-size: var(--nx-font-size-sm); }
/* 右上角外链图标按钮（独立全页运行；与既有 btn-small 同风格，仅收窄内边距） */
.btn-ext { padding: 4px 8px; }
.ext-icon { display: block; }
.btn-primary {
  background: var(--nx-accent);
  color: var(--nx-text-inverse);
  border-color: var(--nx-accent);
}
.btn-primary:hover:not(:disabled) {
  background: var(--nx-accent-hover);
  border-color: var(--nx-accent-hover);
  color: var(--nx-text-inverse);
}
.btn-primary:active:not(:disabled) {
  background: var(--nx-accent-active);
  border-color: var(--nx-accent-active);
}
.btn-primary:disabled {
  background: var(--nx-bg-burn);
  border-color: var(--nx-bg-burn);
  color: var(--nx-text-disabled);
}
.btn-danger { color: var(--nx-danger); border-color: var(--nx-danger-border); }
.btn-danger:hover:not(:disabled) { background: var(--nx-danger-bg); color: var(--nx-danger); }
.spin { display: inline-block; font-size: 14px; line-height: 1; }
.spin.spinning { animation: spin 0.8s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }

/* ===================== 弹窗（三段式钉底：head 固定 + body 滚动 + 操作区 sticky；
     nx 化 = token 卡体 + shadow-lg + divider 头 + 焦点环关闭钮） ===================== */
.modal-backdrop {
  position: fixed;
  inset: 0;
  background: rgb(9 9 11 / 0.42); /* 遮罩：黑系半透明（深浅主题同值） */
  backdrop-filter: blur(3px);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 100;
  padding: 16px;
}
.modal {
  width: min(560px, 100%);
  max-height: 90vh;
  overflow: hidden;
  background: var(--nx-bg-card);
  border: 0.5px solid var(--nx-line-strong);
  border-radius: var(--nx-radius-card);
  box-shadow: var(--nx-shadow-lg);
  display: flex;
  flex-direction: column;
}
.modal-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 16px 20px;
  border-bottom: 0.5px solid var(--nx-line-divider);
  flex-shrink: 0;
}
.modal-head h3 {
  font-size: var(--nx-font-size-md);
  font-weight: var(--nx-font-weight-semibold);
  color: var(--nx-text-primary);
  letter-spacing: -0.01em;
}
.modal-close {
  background: transparent;
  border: none;
  font-size: 24px;
  line-height: 1;
  color: var(--nx-text-tertiary);
  cursor: pointer;
  padding: 0 6px;
  border-radius: var(--nx-radius-input);
  transition: background 0.15s ease, color 0.15s ease;
}
.modal-close:hover { background: var(--nx-bg-hover); color: var(--nx-text-primary); }
.modal-close:focus-visible { outline: 2px solid var(--nx-ring-color); outline-offset: var(--nx-ring-offset); }
.modal-body {
  padding: 18px 20px;
  display: flex;
  flex-direction: column;
  gap: 14px;
  flex: 1 1 auto;
  min-height: 0;
  overflow-y: auto;
}
.modal-body .form-actions {
  position: sticky;
  bottom: -18px;
  margin: 0 -20px -18px;
  padding: 12px 20px;
  background: var(--nx-bg-card);
  border-top: 0.5px solid var(--nx-line-divider);
  display: flex;
  justify-content: flex-end;
  gap: 10px;
}

/* ===================== 窄窗：五区纵向堆叠（监视器置顶——视频预览优先） ===================== */
@media (max-width: 880px) {
  .film-page { overflow-y: auto; }
  .studio-main {
    flex-direction: column;
    min-height: 0;
  }
  .col-preview { order: -1; } /* 监视器（+合成区）最上 */
  .col-timeline,
  .col-shot,
  .col-preview { flex: none; }
  /* 监视器在堆叠态给固定可视高度（画面区不塌缩） */
  .col-preview :deep(.mon) { min-height: 340px; }
  .col-timeline .strip { max-height: 220px; }
  .col-shot .col-body,
  .side-body { overflow: visible; }
  .taskbar-list { max-height: 260px; }
  .char-list { max-height: 300px; }
}

/* 更窄窗口：左侧选项卡栏转顶部横滚条（流程导航保持可达） */
@media (max-width: 720px) {
  .studio-shell { flex-direction: column; }
  .studio-shell :deep(.fh-nav) {
    width: 100%;
    flex-direction: row;
    align-items: center;
    overflow-x: auto;
    overflow-y: hidden;
    flex: none;
  }
  .studio-shell :deep(.fh-nav-item) { flex-shrink: 0; }
  .studio-shell :deep(.fh-nav-sep) { display: none; }
  .studio-shell :deep(.fh-nav.is-collapsed) { width: 100%; }
}
</style>
