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
//   1. 项目列表页：卡片（标题/比例/进度徽章/更新时间）+ 新建对话框（三段式钉底）
//   2. 项目工作室（v0.1.35 FilmHub 流程化）：左侧竖向选项卡栏（SideNav：五流程
//      阶段 ①剧情②分镜③定妆④音频⑤合成 + 工作台 + 设置/成员；阶段徽章读
//      README stage）+ 顶栏（「我是」操作人 + 成本徽章）+ 页面主体：
//      · 五个流程页各自独立组件（src/flow/*Page.vue；FlowContext 共享会话态）
//      · 「工作台」= 原五区（v0.1.35）：左镜头卡纵列 24% + 底部紧凑任务条 /
//        中镜头面板 40%（角色区在面板列内）/ 右预览监视器 36%（PreviewMonitor）
//        + 底部多轨时间轴条（TimelineTracks，四轨 + 播放头，可折叠）——
//        previewEngine.ts provide/inject 共享（final 模式支持 dist 版本文件）。
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
import './flow/flow.css';
import SideNav from './flow/SideNav.vue';
import CostBadge from './flow/CostBadge.vue';
import StoryPage from './flow/StoryPage.vue';
import StoryboardPage from './flow/StoryboardPage.vue';
import CastingPage from './flow/CastingPage.vue';
import AudioPage from './flow/AudioPage.vue';
import ComposePage from './flow/ComposePage.vue';
import SettingsPage from './flow/SettingsPage.vue';
import { FLOW_CONTEXT_KEY, type FlowContext } from './flow/flowContext';
import { parseStageFromMarkdown, type FilmStage, type FlowView } from './flow/flowTypes';
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
  filmCompose,
  filmCreateCharacter,
  filmCreateProject,
  filmDeleteCharacter,
  filmDeleteProject,
  filmGenMusic,
  filmGenPortrait,
  filmGenScript,
  filmGenShotImage,
  filmGenShotTts,
  filmGenShotVideo,
  filmGetFile,
  filmGetProject,
  filmGetTask,
  filmListCharacters,
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
  type FilmCharacter,
  type FilmModelRef,
  type FilmProject,
  type FilmShot,
  type FilmTask,
} from './api';

const { t } = useI18n();

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

/** 画面比例可选项。 */
const RATIOS = ['16:9', '9:16', '1:1'] as const;

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
  /** 联邦中继来源 NodeID（非空 = 🌐 中继渠道）。 */
  via_node?: string;
  [k: string]: unknown;
}

const llmInstances = ref<LlmInstLite[]>([]);
const channels = ref<ChLite[]>([]);
/** 模型源加载错误（不阻断页面——只是下拉里缺对应组）。 */
const srcError = ref('');

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

/** 各能力面选中的下拉 key（'' = 未选；local / llm:<id> / ch:<channel_id>）。 */
const modelSel = reactive<Record<Cap, string>>({
  chat: '',
  image: '',
  video: '',
  tts: '',
  music: '',
});

/** 下拉 key → 契约 model_ref（无效选择返回 null）。 */
function modelRefFor(cap: Cap): FilmModelRef | null {
  const key = modelSel[cap];
  if (!key) return null;
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
// 项目列表页（入口）
// =============================================================================

/** 视图两态：list 项目列表 / studio 项目工作室。 */
const mode = ref<'list' | 'studio'>('list');

const projects = ref<FilmProject[]>([]);
const listLoading = ref(false);
const listError = ref('');

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

/** 「就绪」口径：镜头视频已生成（最终可参与合成；按产物清单判断）。 */
function readyCount(p: FilmProject): number {
  const arts = p.artifacts ?? [];
  return (p.script ?? []).filter((s) => arts.some((a) => a.name === `shot-${s.shot}.mp4`)).length;
}

function fmtTime(iso?: string | null): string {
  if (!iso) return '—';
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString();
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

/** 保存 ownership（PUT files/ownership.json 带作者；成功更新本地态）。 */
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
    return true;
  } catch (e) {
    window.alert(t('film.ownSaveFailed') + errMsg(e));
    return false;
  }
}

/** FlowContext（流程页注入消费：项目/模型源/任务中心/阶段/协作态）。 */
const flowCtx: FlowContext = {
  project,
  optionsFor,
  hasOptionsFor,
  modelSel,
  modelRefFor,
  addTracked,
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
};
provide(FLOW_CONTEXT_KEY, flowCtx);

function enterStudio(p: FilmProject): void {
  mode.value = 'studio';
  project.value = p;
  projectError.value = '';
  selectedShot.value = 1;
  draftDirty.value = false;
  fillDraft();
  navUserPicked.value = false;
  navView.value = 'workbench';
  flowStage.value = '';
  ownership.value = null;
  activity.value = [];
  flowAuthor.value = loadAuthor();
  void reloadProject();
  void loadCharacters();
  // 进入项目即见左侧选项卡布局：读 README 阶段定缺省页（未知时：有分镜回
  // 工作台、无分镜进剧情页——产品流程 hub 建项目 → 剧情页）；用户已点击则不抢
  void loadStage().then(() => {
    if (navUserPicked.value) return;
    if (flowStage.value) navView.value = flowStage.value;
    else navView.value = shots.value.length ? 'workbench' : 'story';
  });
  void refreshCollab();
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
  } catch (e) {
    saveMsg.value = t('film.saveFailed') + errMsg(e);
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

// —— 生成动作（剧本 / 图 / 视频 / 配音 / BGM / 合成）——
const scriptBusy = ref(false);
const scriptError = ref('');
const genBusy = reactive({ image: false, video: false, tts: false });
const shotError = ref('');
const musicBusy = ref(false);
const musicError = ref('');
const musicPrompt = ref('');
const composeBusy = ref(false);
const composeError = ref('');

/** 统一错误文案（film.rs 未就绪时 404/405 给出友好口径）。 */
function errMsg(e: unknown): string {
  const m = e instanceof Error ? e.message : String(e);
  if (/404|405|not found|method not allowed/i.test(m)) {
    return `${m}（film 后端可能尚未就绪）`;
  }
  return m;
}

/** 生成类动作公共骨架：校验 model_ref → 提交 → 任务进任务中心（轮询）。 */
async function submitGen(
  cap: Cap,
  errRef: { value: string },
  run: (ref: FilmModelRef) => Promise<FilmTask>,
): Promise<void> {
  const ref = modelRefFor(cap);
  if (!ref) {
    errRef.value = hasOptionsFor(cap) ? t('film.pickModel') : t('film.noSource');
    return;
  }
  errRef.value = '';
  try {
    const task = await run(ref);
    addTracked(task);
  } catch (e) {
    errRef.value = t('film.actFailed') + errMsg(e);
  }
}

async function genScript(): Promise<void> {
  if (!project.value || scriptBusy.value) return;
  if (shots.value.length && !window.confirm(t('film.genScriptHint'))) return;
  scriptBusy.value = true;
  try {
    await submitGen('chat', scriptError, (ref) =>
      filmGenScript(project.value!.id, ref),
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

async function genMusic(): Promise<void> {
  if (!project.value || musicBusy.value) return;
  musicBusy.value = true;
  try {
    await submitGen('music', musicError, (ref) =>
      filmGenMusic(project.value!.id, ref, musicPrompt.value.trim() || undefined),
    );
  } finally {
    musicBusy.value = false;
  }
}

async function composeFinal(): Promise<void> {
  const cur = project.value;
  if (!cur || composeBusy.value) return;
  composeBusy.value = true;
  composeError.value = '';
  try {
    const task = await filmCompose(cur.id);
    addTracked(task);
  } catch (e) {
    composeError.value = t('film.actFailed') + errMsg(e);
  } finally {
    composeBusy.value = false;
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
  settingsSaving.value = true;
  settingsError.value = '';
  try {
    const fresh = await filmUpdateProject(cur.id, { export_dir: exportDirForm.value.trim() });
    // 保守合并：响应回显 export_dir/final_path（旧后端缺字段保留旧值）
    project.value = {
      ...cur,
      ...fresh,
      script: fresh.script ?? cur.script,
      artifacts: fresh.artifacts ?? cur.artifacts,
    };
    showSettings.value = false;
  } catch (e) {
    // 400 等：红条直显后端校验信息（不做前缀拼接，保留原始校验文案）
    settingsError.value = errMsg(e);
  } finally {
    settingsSaving.value = false;
  }
}

// =============================================================================
// 任务中心（FilmTask 轮询：2s，完成刷新项目产物）
// =============================================================================

/** 任务中心条目（tracked = 已进入 UI 的任务）。 */
interface TrackedTask {
  id: string;
  kind: string;
  shot: number | null;
  status: string;
  lastLog: string;
  error: string;
  /** 终态（completed/failed）——不再轮询。 */
  done: boolean;
}

const trackedTasks = ref<TrackedTask[]>([]);
const POLL_MS = 2000;
let pollTimer: ReturnType<typeof setInterval> | null = null;

// —— 紧凑任务条（左栏底部；任务中心 v0.1.35 从右栏迁来，功能不丢：
//    收起=一行「任务 N 进行中」摘要，展开=完整列表 + 环形日志尾）——
const tasksOpen = ref(false);
/** 进行中（非终态）任务数。 */
const activeTaskCount = computed(() => trackedTasks.value.filter((x) => !x.done).length);

function addTracked(task: FilmTask): void {
  trackedTasks.value.unshift({
    id: task.id,
    // 后端字段是 stage（kind 为旧契约别名）——202 响应即取，防 undefined 渲染崩
    kind: (task as unknown as { stage?: string }).stage ?? task.kind ?? '',
    shot: null,
    status: task.status,
    lastLog: (task.log ?? [])[0] ?? '',
    error: task.error ?? '',
    done: task.status === 'completed' || task.status === 'failed',
  });
  startPolling();
}

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
      item.lastLog = log.length ? log[log.length - 1] : item.lastLog;
      item.error = task.error ?? '';
      if (item.status === 'completed' || item.status === 'failed') {
        item.done = true;
        finished = true;
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
    void loadStage();
    void refreshCollab();
    refreshTick.value++;
  }
  if (trackedTasks.value.every((x) => x.done)) stopPolling();
}

/** 任务中心条目标签（kind + 关联镜头）。 */
function taskLabel(item: TrackedTask): string {
  const kind = item.kind || '';
  const kindKey = `film.k${kind.charAt(0).toUpperCase()}${kind.slice(1)}`;
  const kindText = ['script', 'image', 'video', 'tts', 'music', 'compose', 'portrait'].includes(
    kind,
  )
    ? t(kindKey)
    : kind;
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

/** 返回项目列表（任务轮询不中断——到终态自动停，完成刷新已被 mode 守卫）。 */
function backToList(): void {
  mode.value = 'list';
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
  } catch (e) {
    charError.value = t('film.charSaveFailed') + errMsg(e);
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
  } catch (e) {
    window.alert(t('film.charDelFailed') + errMsg(e));
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
  } catch (err) {
    charsError.value = t('film.charUploadFailed') + errMsg(err);
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
  } catch (e) {
    saveMsg.value = t('film.saveFailed') + errMsg(e);
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
  } catch (err) {
    charsError.value = t('film.refUploadFailed') + errMsg(err);
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
  }
}

// =============================================================================
// 生命周期
// =============================================================================
onMounted(() => {
  void initCaps(); // 能力快照先行（独立模式顶栏徽章数据源）
  void loadProjects();
  void loadModelSources();
});

onUnmounted(() => {
  stopPolling();
  previewEngine.dispose();
  unsubCaps?.();
  unsubCaps = null;
});

// 模型源加载完成前用户已打开下拉的兜底：srcError 出现/清除时补默认项
watch(srcError, () => pickDefaultModels());
</script>

<template>
  <div class="film-page">
    <!-- ==================== 项目列表页（入口） ==================== -->
    <template v-if="mode === 'list'">
      <div class="page-head">
        <div>
          <h2 class="page-title">{{ t('film.title') }}</h2>
          <div class="page-sub muted">{{ t('film.subtitle') }}</div>
        </div>
        <div class="head-actions">
          <!-- 能力徽章（独立模式；全能力=无徽章） -->
          <span
            v-if="isStandalone && deg && deg.mode !== 'full'"
            class="caps-badge"
            :class="deg.mode === 'offline' ? 'caps-off' : 'caps-deg'"
            :title="deg.mode === 'offline'
              ? t('film.capsOfflineTip')
              : t('film.capsMissingTip', { caps: deg.missing.join(', ') })"
          >{{ deg.mode === 'offline' ? t('film.capsOffline') : t('film.capsDegraded') }}</span>
          <button class="btn btn-small" :disabled="listLoading" @click="loadProjects">
            <span class="spin" :class="{ spinning: listLoading }" aria-hidden="true">↻</span>
            {{ t('film.refresh') }}
          </button>
          <button class="btn btn-primary btn-small" @click="openCreate">
            + {{ t('film.newProject') }}
          </button>
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

      <div v-if="srcError" class="warn-box">{{ t('film.srcFailed') }}{{ srcError }}</div>
      <div v-if="listError" class="error-box">
        {{ t('film.listFailed') }}{{ listError }}
        <button class="btn btn-small" @click="loadProjects">{{ t('film.retry') }}</button>
      </div>

      <div class="list-scroll">
        <div v-if="!listLoading && projects.length === 0" class="card empty-state">
          <div class="empty-icon">🎬</div>
          <div class="empty-text">{{ t('film.emptyTitle') }}</div>
          <div class="empty-hint muted">{{ t('film.emptyHint') }}</div>
        </div>
        <div v-else class="proj-grid">
          <div
            v-for="p in projects"
            :key="p.id"
            class="card proj-card"
            @click="enterStudio(p)"
          >
            <div class="proj-card-head">
              <span class="proj-title" :title="p.title">{{ p.title }}</span>
              <span class="pill pill-ratio mono">{{ p.ratio }}</span>
            </div>
            <p class="proj-idea muted">{{ p.idea }}</p>
            <div class="proj-card-foot">
              <span
                v-if="(p.script ?? []).length"
                class="pill"
                :class="readyCount(p) === (p.script ?? []).length ? 'pill-ok' : 'pill-blue'"
              >
                {{ t('film.shotsReady', { ready: readyCount(p), total: (p.script ?? []).length }) }}
              </span>
              <span v-else class="pill pill-muted">{{ t('film.noShots') }}</span>
              <span class="proj-time muted">{{ t('film.updatedAt', { time: fmtTime(p.updated_at) }) }}</span>
            </div>
            <div class="proj-actions" @click.stop>
              <button class="btn btn-small btn-primary" @click="enterStudio(p)">
                {{ t('film.open') }}
              </button>
              <button class="btn btn-small btn-danger" @click="removeProject(p)">
                {{ t('film.del') }}
              </button>
            </div>
          </div>
        </div>
      </div>
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
          <span class="pill pill-ratio mono">{{ project?.ratio }}</span>
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
          <!-- 生成剧本（工作台视图内；流程化后分镜生成移至分镜页） -->
          <template v-if="navView === 'workbench'">
            <select v-model="modelSel.chat" class="msel-select" :title="t('film.model')">
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
              :disabled="scriptBusy || !project || !hasOptionsFor('chat') || !chatAvailable"
              :title="genDisabledTip('script') ?? t('film.genScriptHint')"
              @click="genScript"
            >
              {{ scriptBusy ? t('film.taskRunning') + '…' : t('film.genScript') }}
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
      <div v-if="projectError" class="error-box">
        {{ projectError }}
        <button class="btn btn-small" @click="reloadProject">{{ t('film.retry') }}</button>
      </div>

      <!-- 主体（v0.1.35 FilmHub 流程化）：左侧选项卡栏（SideNav：五阶段 + 工作台
           + 设置/成员；阶段徽章读 README stage）+ 页面主体（流程页各自独立组件，
           FlowContext 共享会话态；「工作台」= 原五区） -->
      <div class="studio-shell">
        <SideNav :view="navView" :stage="flowStage" @select="setFlowView" />
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
              <span class="pill pill-muted taskbar-count mono">{{ trackedTasks.length }}</span>
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
                  <span class="pill" :class="item.status === 'completed' ? 'pill-ok' : item.status === 'failed' ? 'pill-err' : 'pill-blue'">
                    {{ taskStatusLabel(item.status) }}
                  </span>
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

              <!-- 生成动作：图（本地 sd-turbo + 渠道）/ 视频 / 配音（仅渠道） -->
              <div v-if="shotError" class="error-box error-box-slim">{{ shotError }}</div>
              <div class="gen-row">
                <div class="gen-cell">
                  <select v-model="modelSel.image" class="msel-select" :title="t('film.model')">
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
                    :disabled="genBusy.image || !hasOptionsFor('image') || isOffline"
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
                    :disabled="genBusy.video || !hasOptionsFor('video') || !channelAvailable"
                    :title="genDisabledTip('media') ?? (hasOptionsFor('video') ? '' : t('film.channelOnlyHint'))"
                    @click="genVideo"
                  >
                    {{ genBusy.video ? '…' : t('film.genVideo') }}
                  </button>
                  <span v-if="refInjectCount" class="gen-hint" :title="t('film.refInjectTip')">
                    🧩 {{ t('film.refInject', { n: refInjectCount }) }}
                  </span>
                </div>
                <div class="gen-cell">
                  <select v-model="modelSel.tts" class="msel-select" :title="t('film.model')">
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
                    :disabled="genBusy.tts || !hasOptionsFor('tts') || !channelAvailable"
                    :title="genDisabledTip('media') ?? (hasOptionsFor('tts') ? '' : t('film.channelOnlyHint'))"
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

          <!-- 紧凑合成区（监视器下） -->
          <div class="card side-block compose-compact">
            <div class="col-head">{{ t('film.composeArea') }}</div>
            <div class="side-body">
              <label class="field">
                <span class="field-label">
                  🎵 {{ t('film.bgmPrompt') }}
                  <span v-if="hasArtifact('bgm.mp3')" class="pill pill-ok pill-mini">✅ bgm.mp3</span>
                </span>
                <input
                  v-model="musicPrompt"
                  type="text"
                  :placeholder="t('film.bgmPromptPh')"
                >
              </label>
              <div class="gen-cell">
                <select v-model="modelSel.music" class="msel-select" :title="t('film.model')">
                  <option v-if="!hasOptionsFor('music')" value="" disabled>
                    {{ t('film.channelOnlyHint') }}
                  </option>
                  <optgroup v-for="g in optionsFor('music')" :key="g.label" :label="g.label">
                    <option v-for="o in g.options" :key="o.key" :value="o.key">
                      {{ o.label }}{{ o.relay ? ' 🌐' : '' }}
                    </option>
                  </optgroup>
                </select>
                <button
                  class="btn btn-small"
                  :disabled="musicBusy || !hasOptionsFor('music') || !channelAvailable"
                  :title="genDisabledTip('media') ?? (hasOptionsFor('music') ? '' : t('film.channelOnlyHint'))"
                  @click="genMusic"
                >
                  {{ musicBusy ? '…' : t('film.genMusic') }}
                </button>
              </div>
              <div v-if="musicError" class="error-box error-box-slim">{{ musicError }}</div>

              <div class="compose-row">
                <button
                  class="btn btn-primary btn-small compose-btn"
                  :disabled="composeBusy || !project || !composeAvailable"
                  :title="genDisabledTip('compose')"
                  @click="composeFinal"
                >
                  {{ composeBusy ? t('film.taskRunning') + '…' : t('film.compose') }}
                </button>
                <!-- final.mp4 已生成：监视器切全片播放模式（再点返回分镜预览） -->
                <button
                  v-if="hasArtifact('final.mp4')"
                  class="btn btn-small"
                  :class="{ 'btn-final-on': previewingFinal }"
                  :title="hasArtifact('final.mp4') ? t('film.finalTitle') : t('film.finalEmpty')"
                  @click="toggleFinalPreview"
                >🎬 {{ previewingFinal ? t('film.previewStoryboard') : t('film.previewFinal') }}</button>
                <button
                  v-if="hasArtifact('final.mp4')"
                  class="btn btn-small"
                  @click="downloadFinal"
                >⬇ {{ t('film.download') }}</button>
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
          <div class="field-row">
            <label class="field field-narrow">
              <span class="field-label">{{ t('film.fRatio') }}</span>
              <select v-model="createForm.ratio" :disabled="creating">
                <option v-for="r in RATIOS" :key="r" :value="r">{{ r }}</option>
              </select>
            </label>
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
  </div>
</template>

<style scoped>
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
.page-head {
  display: flex;
  justify-content: space-between;
  align-items: center;
  gap: 12px;
  flex-wrap: wrap;
  flex-shrink: 0;
}
.page-title {
  font-size: 22px;
  font-weight: 700;
  color: var(--text, #2B2B2B);
  letter-spacing: -0.02em;
}
.page-sub {
  margin-top: 4px;
  font-size: 13px;
}
.head-actions {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}
.muted { color: var(--text-muted, #5E5C5F); }
.small { font-size: 12.5px; }
.mono { font-family: var(--mono, monospace); }

/* 列表滚动区 */
.list-scroll {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
}
.proj-grid {
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(min(300px, 100%), 1fr));
  gap: 14px;
}
.card {
  background: var(--bg-card, #ffffff);
  border: 1px solid var(--border, #D9D9D9);
  border-radius: var(--radius-md, 12px);
  box-shadow: var(--shadow, 0 1px 3px rgba(0, 0, 0, 0.1));
}

/* 项目卡 */
.proj-card {
  padding: 14px 16px;
  display: flex;
  flex-direction: column;
  gap: 8px;
  cursor: pointer;
  transition: border-color 0.15s ease;
}
.proj-card:hover { border-color: var(--accent, #E95420); }
.proj-card-head {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
}
.proj-title {
  font-size: 15px;
  font-weight: 700;
  color: var(--text, #2B2B2B);
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  flex: 1;
}
.proj-idea {
  margin: 0;
  font-size: 12.5px;
  line-height: 1.55;
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
  min-height: 2.9em;
}
.proj-card-foot {
  display: flex;
  align-items: center;
  gap: 8px;
  flex-wrap: wrap;
}
.proj-time { font-size: 12px; margin-left: auto; }
.proj-actions {
  display: flex;
  gap: 8px;
  border-top: 1px solid var(--border-soft, #EDEDED);
  padding-top: 10px;
}

/* 空态 */
.empty-state {
  padding: 48px 20px;
  text-align: center;
  display: flex;
  flex-direction: column;
  align-items: center;
  gap: 8px;
}
.empty-icon { font-size: 44px; }
.empty-text { font-size: 16px; font-weight: 600; color: var(--text, #2B2B2B); }
.empty-hint { font-size: 13px; }

/* pill 徽章 */
.pill {
  display: inline-flex;
  align-items: center;
  padding: 2px 10px;
  border-radius: var(--radius-pill, 20px);
  font-size: 12px;
  border: 1px solid var(--border, #D9D9D9);
  background: var(--border-soft, #FAFAFA);
  color: var(--text, #2B2B2B);
  white-space: nowrap;
  flex-shrink: 0;
}
.pill-ok { color: #15803d; background: #f0fdf4; border-color: #bbf7d0; }
.pill-err { color: #b91c1c; background: #fef2f2; border-color: #fecaca; }
.pill-blue { color: #1d4ed8; background: #eff6ff; border-color: #bfdbfe; }
.pill-muted { color: var(--text-muted, #5E5C5F); }
.pill-ratio { font-size: 11.5px; padding: 1px 8px; }

/* 能力徽章（@nexos/app-sdk 降级三态；全能力=不渲染） */
.caps-badge {
  display: inline-flex;
  align-items: center;
  padding: 2px 10px;
  border-radius: var(--radius-pill, 20px);
  font-size: 12px;
  white-space: nowrap;
  flex-shrink: 0;
  cursor: help;
}
.caps-deg {
  color: #92400e;
  background: #fffbeb;
  border: 1px solid #fde68a;
}
.caps-off {
  color: #b91c1c;
  background: #fef2f2;
  border: 1px solid #fecaca;
}

/* ===================== 工作室：顶栏 ===================== */
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
  font-size: 16px;
  font-weight: 700;
  color: var(--text, #2B2B2B);
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
.col-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  gap: 8px;
  padding: 10px 14px;
  border-bottom: 1px solid var(--border-soft, #EDEDED);
  font-size: 14px;
  font-weight: 600;
  color: var(--text, #2B2B2B);
  flex-shrink: 0;
}
.col-body {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: 12px 14px;
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
/* ① 紧凑任务条（任务中心从右栏迁来；收起=一行摘要，展开=列表+日志尾） */
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
  font-size: 12.5px;
  font-weight: 600;
  color: var(--text, #2B2B2B);
  cursor: pointer;
  text-align: left;
}
.taskbar-head:hover { background: rgba(0, 0, 0, 0.03); }
.taskbar-title {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.taskbar-count { flex-shrink: 0; font-size: 10.5px; padding: 0 7px; }
.taskbar-caret { color: var(--text-muted, #5E5C5F); font-size: 11px; flex-shrink: 0; }
.taskbar-list {
  max-height: 240px;
  overflow-y: auto;
  padding: 8px 10px;
  border-top: 1px solid var(--border-soft, #EDEDED);
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
  font-size: 13px;
  text-align: center;
  width: 100%;
}
.shot-card {
  flex: 0 0 140px;
  display: flex;
  flex-direction: column;
  gap: 4px;
  padding: 8px 10px;
  border: 1.5px solid var(--border, #D9D9D9);
  border-radius: var(--radius-sm, 10px);
  background: var(--bg-card, #fff);
  cursor: pointer;
  text-align: left;
  font-family: inherit;
  font-size: 12px;
  color: var(--text, #2B2B2B);
  transition: border-color 0.15s ease, background 0.15s ease;
}
.shot-card:hover { border-color: var(--accent, #E95420); }
.shot-card.active {
  border-color: var(--accent, #E95420);
  background: var(--accent-soft, rgba(233, 84, 32, 0.08));
}
.shot-no { font-weight: 700; font-size: 13px; }
.shot-state { font-size: 11.5px; color: var(--text-muted, #5E5C5F); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.shot-desc {
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
  line-height: 1.45;
  min-height: 2.9em;
}
.shot-arts { display: flex; gap: 6px; }
.art-dot { opacity: 0.25; font-size: 11px; }
.art-dot.on { opacity: 1; }

/* ② 镜头面板表单 */
.field {
  display: flex;
  flex-direction: column;
  gap: 4px;
  min-width: 0;
}
.field-grow { flex: 1; }
.field-narrow { width: 130px; flex-shrink: 0; }
.field-label {
  font-size: 12px;
  font-weight: 600;
  color: var(--text-muted, #5E5C5F);
}
.field input,
.field textarea,
.field select {
  font-family: inherit;
  font-size: 13px;
  padding: 6px 9px;
  border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px);
  background: var(--bg-card, #fff);
  color: var(--text, #2B2B2B);
  resize: vertical;
  min-width: 0;
}
.field-row {
  display: flex;
  gap: 10px;
  align-items: flex-start;
  flex-wrap: wrap;
}
.field-input {
  font-family: inherit;
  font-size: 13px;
  padding: 5px 8px;
  border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px);
  background: var(--bg-card, #fff);
  color: var(--text, #2B2B2B);
  min-width: 0;
  flex: 1;
}
.save-row {
  display: flex;
  align-items: center;
  gap: 10px;
}
.save-msg { font-size: 12.5px; color: #15803d; }
.save-msg.err { color: #b91c1c; }

/* 生成行：每格 = 模型选择器 + 按钮 */
.gen-row {
  display: flex;
  flex-direction: column;
  gap: 8px;
  border-top: 1px dashed var(--border, #D9D9D9);
  padding-top: 10px;
}
.gen-cell {
  display: flex;
  gap: 8px;
  align-items: center;
  min-width: 0;
}
.gen-cell .msel-select { flex: 1; min-width: 0; }
.msel-select {
  font-family: inherit;
  font-size: 12.5px;
  padding: 5px 8px;
  border: 1px solid var(--border, #d1d5db);
  border-radius: var(--radius-sm, 8px);
  background: var(--bg-card, #fff);
  color: var(--text, #2B2B2B);
  max-width: 100%;
}

/* 产物预览 */
.arts {
  display: flex;
  flex-direction: column;
  gap: 10px;
  border-top: 1px dashed var(--border, #D9D9D9);
  padding-top: 10px;
}
.art-block {
  display: flex;
  flex-direction: column;
  gap: 6px;
  min-width: 0;
}
.art-title { font-size: 12.5px; font-weight: 600; color: var(--text-muted, #5E5C5F); }
.art-img {
  max-width: 100%;
  max-height: 220px;
  border-radius: var(--radius-sm, 8px);
  border: 1px solid var(--border-soft, #EDEDED);
  object-fit: contain;
  align-self: flex-start;
}
.art-video {
  width: 100%;
  max-height: 220px;
  border-radius: var(--radius-sm, 8px);
  background: #000;
}
.art-audio { width: 100%; height: 36px; }
.art-none {
  font-size: 12.5px;
  padding: 10px 12px;
  border: 1px dashed var(--border, #D9D9D9);
  border-radius: var(--radius-sm, 8px);
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
/* 「预览成片」激活态（final 模式中 → 橙底白字示意可切回） */
.btn-final-on {
  background: var(--accent-soft, rgba(233, 84, 32, 0.12));
  border-color: var(--accent, #E95420);
  color: var(--accent, #E95420);
}

/* 角色库：工具行 + 角色卡列表 */
.char-toolbar {
  display: flex;
  gap: 8px;
  padding: 8px 10px;
  border-bottom: 1px solid var(--border-soft, #EDEDED);
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
.char-card {
  border: 1px solid var(--border-soft, #EDEDED);
  border-radius: var(--radius-sm, 8px);
  padding: 8px 10px;
  display: flex;
  gap: 10px;
  font-size: 12.5px;
}
.char-thumb {
  width: 48px;
  height: 48px;
  flex-shrink: 0;
  border-radius: var(--radius-sm, 8px);
  border: 1px solid var(--border-soft, #EDEDED);
  overflow: hidden;
  display: flex;
  align-items: center;
  justify-content: center;
  background: var(--border-soft, #FAFAFA);
}
.char-thumb img { width: 100%; height: 100%; object-fit: cover; }
.char-thumb-ph { font-size: 22px; opacity: 0.5; }
.char-main { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 2px; }
.char-row1 { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
.char-name { font-weight: 700; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.pill-voice { font-size: 11px; padding: 0 8px; color: #6d28d9; background: #f5f3ff; border-color: #ddd6fe; }
.char-desc {
  display: -webkit-box;
  -webkit-line-clamp: 2;
  -webkit-box-orient: vertical;
  overflow: hidden;
  font-size: 12px;
  line-height: 1.45;
}
.char-meta { font-size: 11.5px; }
.char-actions {
  display: flex;
  flex-direction: column;
  gap: 4px;
  flex-shrink: 0;
}
.char-actions .btn { padding: 2px 8px; font-size: 11.5px; }

/* 参考图缩略条 */
.ref-strip {
  border-top: 1px solid var(--border-soft, #EDEDED);
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
  border-radius: 6px;
  border: 1px solid var(--border-soft, #EDEDED);
  overflow: hidden;
  display: flex;
  align-items: center;
  justify-content: center;
  background: var(--border-soft, #FAFAFA);
}
.ref-item img { width: 100%; height: 100%; object-fit: cover; }
.hidden-input { display: none; }

/* 镜头面板：角色绑定 chips */
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
  border-radius: var(--radius-pill, 20px);
  font-size: 12px;
  background: var(--accent-soft, rgba(233, 84, 32, 0.08));
  border: 1px solid var(--accent, #E95420);
  color: var(--text, #2B2B2B);
}
.chip-x {
  background: transparent;
  border: none;
  font-size: 13px;
  line-height: 1;
  color: var(--text-muted, #5E5C5F);
  cursor: pointer;
  padding: 0 4px;
}
.chip-x:hover { color: #b91c1c; }
.chip-add {
  font-family: inherit;
  font-size: 12px;
  padding: 3px 8px;
  border: 1px dashed var(--border, #d1d5db);
  border-radius: var(--radius-pill, 20px);
  background: var(--bg-card, #fff);
  color: var(--text-muted, #5E5C5F);
}

/* 生成行注入提示 */
.gen-hint {
  font-size: 11.5px;
  color: var(--text-muted, #5E5C5F);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
  flex-shrink: 0;
}
/* 产物已生成状态行 */
.art-ready {
  font-size: 12.5px;
  color: #15803d;
  padding: 10px 12px;
  border: 1px dashed #bbf7d0;
  border-radius: var(--radius-sm, 8px);
  background: #f0fdf4;
}

/* 任务条目（紧凑任务条展开列表复用；v0.1.35 从右栏任务中心迁来） */
.task-item {
  border: 1px solid var(--border-soft, #EDEDED);
  border-radius: var(--radius-sm, 8px);
  padding: 8px 10px;
  display: flex;
  flex-direction: column;
  gap: 4px;
  font-size: 12.5px;
}
.task-item.is-failed { border-color: #fecaca; background: #fef2f2; }
.task-row1 {
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 0;
}
.task-kind { font-weight: 600; flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.task-dismiss {
  background: transparent;
  border: none;
  font-size: 16px;
  line-height: 1;
  color: var(--text-muted, #5E5C5F);
  cursor: pointer;
  padding: 0 2px;
}
.task-log {
  font-size: 11px;
  color: var(--text-muted, #5E5C5F);
  white-space: nowrap;
  overflow: hidden;
  text-overflow: ellipsis;
}
.task-err { font-size: 11.5px; color: #b91c1c; word-break: break-all; }
.side-body {
  flex: 1;
  min-height: 0;
  overflow-y: auto;
  padding: 12px 14px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.compose-row {
  display: flex;
  gap: 8px;
  align-items: center;
  flex-wrap: wrap;
  border-top: 1px dashed var(--border, #D9D9D9);
  padding-top: 10px;
}
.compose-btn { flex: 1; }
.art-final { max-height: 180px; }

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
/* 导出设置弹窗：当前成片路径展示行 */
.final-path-row {
  font-size: 12px;
  padding: 8px 10px;
  border: 1px dashed var(--border, #D9D9D9);
  border-radius: var(--radius-sm, 8px);
  background: var(--border-soft, #FAFAFA);
  color: var(--text, #2B2B2B);
  word-break: break-all;
}

/* ===================== 消息条 ===================== */
.error-box {
  color: #b91c1c;
  background: #fee2e2;
  border: 1px solid rgba(185, 28, 28, 0.2);
  padding: 10px 14px;
  border-radius: var(--radius-sm, 8px);
  font-size: 13px;
  flex-shrink: 0;
  display: flex;
  align-items: center;
  gap: 10px;
  flex-wrap: wrap;
  word-break: break-all;
}
.error-box-slim { padding: 6px 10px; font-size: 12px; }
.warn-box {
  color: #92400e;
  background: #fffbeb;
  border: 1px solid rgba(245, 158, 11, 0.4);
  padding: 8px 14px;
  border-radius: var(--radius-sm, 8px);
  font-size: 12.5px;
  flex-shrink: 0;
}

/* ===================== 按钮 ===================== */
.btn {
  padding: 6px 14px;
  border-radius: var(--radius-sm, 8px);
  border: 1px solid var(--border, #d1d5db);
  background: var(--bg-card, #ffffff);
  color: var(--text, #2B2B2B);
  font-size: 13px;
  cursor: pointer;
  font-family: inherit;
  text-decoration: none;
  display: inline-flex;
  align-items: center;
  gap: 4px;
  justify-content: center;
  transition: background 0.15s ease, opacity 0.15s ease;
  flex-shrink: 0;
}
.btn:hover { background: rgba(0, 0, 0, 0.04); }
.btn:disabled { opacity: 0.5; cursor: not-allowed; }
.btn-small { padding: 4px 10px; font-size: 12.5px; }
/* 右上角外链图标按钮（独立全页运行；与既有 btn-small 同风格，仅收窄内边距） */
.btn-ext { padding: 4px 8px; }
.ext-icon { display: block; }
.btn-primary {
  background: var(--accent, #E95420);
  color: #ffffff;
  border-color: var(--accent, #E95420);
}
.btn-primary:hover:not(:disabled) { background: var(--accent-hi, #0077ed); }
.btn-danger { color: #b91c1c; border-color: #fecaca; }
.btn-danger:hover { background: #fef2f2; }
.spin { display: inline-block; font-size: 14px; line-height: 1; }
.spin.spinning { animation: spin 0.8s linear infinite; }
@keyframes spin { to { transform: rotate(360deg); } }

/* ===================== 新建项目弹窗（三段式钉底：head 固定 + body 滚动 + 操作区 sticky） ===================== */
.modal-backdrop {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.35);
  backdrop-filter: blur(2px);
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
  background: var(--bg-card, #fff);
  border-radius: var(--radius-md, 16px);
  box-shadow: 0 20px 60px rgba(0, 0, 0, 0.25);
  display: flex;
  flex-direction: column;
}
.modal-head {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 16px 20px;
  border-bottom: 1px solid var(--border-soft, #EDEDED);
  flex-shrink: 0;
}
.modal-head h3 { font-size: 16px; font-weight: 600; }
.modal-close {
  background: transparent;
  border: none;
  font-size: 24px;
  line-height: 1;
  color: var(--text-muted, #5E5C5F);
  cursor: pointer;
  padding: 0 6px;
}
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
  background: var(--bg-card, #fff);
  border-top: 1px solid var(--border-soft, #EDEDED);
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
  .studio-shell :deep(.fh-nav-hint),
  .studio-shell :deep(.fh-nav-sep) { display: none; }
  .studio-shell :deep(.fh-nav.is-collapsed) { width: 100%; }
}
</style>
