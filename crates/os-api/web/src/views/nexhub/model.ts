// =============================================================================
// nexhub/model.ts —— CodeHub 共享数据模型与格式化工具（v0.1.32 拆分自 CodeHub.vue）。
//
// 类型对齐后端 DTO：
//   /api/v1/coderepo/*   （CodeRepoRouteHandler，os-nexhub code_repo.rs）
//   /api/v1/nexhub/lobby/*（NexHubLobbyRouteHandler，os-nexhub nexhub_lobby.rs）
//   /api/v1/apps/catalog （apps_handler.rs，应用包目录——NexHub DeployButton 数据源）
// =============================================================================
import i18n from '@/i18n';

/** 裸仓库（GET /api/v1/coderepo/repos 的 repos 元素）。 */
export interface Repo {
  name?: string;
  description?: string;
  size_bytes?: number;
  last_commit?: string | null;
  last_commit_date?: string | null;
  branch_count?: number;
  commit_count?: number;
  clone_url_ssh?: string;
  clone_url_http?: string | null;
}

/** 提交（GET /api/v1/coderepo/repos/:name/commits 的 commits 元素）。 */
export interface CommitInfo {
  hash?: string;
  author?: string;
  message?: string;
  date?: string;
}

/** AI 会话归档记录（GET /api/v1/coderepo/sessions 元素）。 */
export interface AgentSession {
  id?: string;
  agent_name?: string;
  repo_name?: string;
  session_summary?: string;
  files_changed?: number;
  commits?: number;
  started_at?: string;
  ended_at?: string | null;
}

/** 文件树节点（GET /api/v1/coderepo/repos/:name/contents 的 tree 元素）。 */
export interface FileTreeNode {
  name?: string;
  path?: string;
  is_dir?: boolean;
  size?: number | null;
}

/** 仓库统计（GET /api/v1/coderepo/stats）。 */
export interface Stats {
  repo_count?: number;
  total_size?: number;
  session_count?: number;
  total_commits?: number;
}

/** 大厅条目（GET /api/v1/nexhub/lobby）。 */
export interface LobbyEntry {
  repo_name?: string;
  description?: string;
  tags?: string[];
  publisher?: string;
  source_url?: string;
  /** 联邦来源节点（P3）：本地发布恒 'local'；远程条目 = 发布节点名（🌐 徽章）。 */
  source_node?: string;
  commit_count?: number;
  size_bytes?: number;
  default_branch?: string;
  last_commit?: string | null;
  last_commit_date?: string | null;
  readme_excerpt?: string;
  download_count?: number;
  published_at?: string;
  /** 价格（最小货币单位；0=免费）。付费条目克隆前需先 purchase。 */
  price_sats?: number;
  /** free / btc / nex / usdc / eth。 */
  currency?: string;
  /** 是否已推送到联邦大厅（两步联邦：本地发布 → POST /:name/federate 推送）。 */
  federated?: boolean;
  // 详情接口额外返回的双通道 clone 地址（列表接口不含）
  clone_url_ssh?: string;
  clone_url_http?: string;
}

/** 大厅统计（GET /api/v1/nexhub/lobby/stats）。 */
export interface LobbyStats {
  published_count?: number;
  total_downloads?: number;
  top_tags?: { tag: string; count: number }[];
}

/** 文件树拍平节点（树 → 缩进列表渲染）。 */
export interface FlatNode {
  node: FileTreeNode;
  depth: number;
}

/** 应用仓库判定（与后端 CATALOG_REPO_PREFIX 同约定：nexos-app-* 前缀）。 */
export const APP_REPO_PREFIX = 'nexos-app-';

export function isAppRepo(name?: string): boolean {
  return !!name && name.startsWith(APP_REPO_PREFIX);
}

/** 全局提示条（页头下方 form-msg）。 */
export interface HubMsg {
  kind: 'info' | 'error' | 'ok';
  text: string;
}

// =============================================================================
// 格式化 / 小工具（拆分前 CodeHub.vue 内联实现，原样迁移）
// =============================================================================
export function errMsg(e: unknown): string {
  if (e instanceof Error) return e.message;
  return String(e);
}

export function formatBytes(b?: number): string {
  if (!b || b <= 0) return '0 B';
  const kb = b / 1024;
  if (kb < 1) return `${b} B`;
  if (kb < 1024) return `${kb.toFixed(0)} KB`;
  const mb = kb / 1024;
  if (mb < 1024) return `${mb.toFixed(1)} MB`;
  return `${(mb / 1024).toFixed(2)} GB`;
}

export function formatDate(s?: string | null): string {
  if (!s) return '—';
  try {
    return new Date(s).toLocaleString(i18n.global.locale.value);
  } catch {
    return s;
  }
}

/** 相对时间（"刚刚 / N 分钟前 / N 小时前 / N 天前"，超 30 天回退日期）。 */
export function formatRelative(s?: string | null): string {
  if (!s) return '—';
  const t = new Date(s).getTime();
  if (Number.isNaN(t)) return s;
  const diff = Date.now() - t;
  const MIN = 60_000;
  const HOUR = 3_600_000;
  const DAY = 86_400_000;
  const gt = i18n.global.t;
  if (diff < MIN) return gt('nexhub.time.justNow');
  if (diff < HOUR) return gt('nexhub.time.minAgo', { n: Math.floor(diff / MIN) });
  if (diff < DAY) return gt('nexhub.time.hourAgo', { n: Math.floor(diff / HOUR) });
  if (diff < 30 * DAY) return gt('nexhub.time.dayAgo', { n: Math.floor(diff / DAY) });
  return formatDate(s);
}

export function shortHash(h?: string): string {
  return h ? h.slice(0, 7) : '';
}

export function agentColor(name?: string): string {
  switch (name) {
    case 'zcode':
      return '#E95420';
    case 'claude-code':
      return '#D97757';
    case 'codex':
      return '#10a37f';
    case 'cursor':
      return '#5b86e5';
    case 'aider':
      return '#22c55e';
    default:
      return '#772953';
  }
}

/**
 * 语义化版本比较（v 前缀容忍；按 '.' 段数值逐段比较，短段补 0）。
 * DeployButton 升级判定：catalog 版本 > installed_version 时显示「升级」。
 * 返回 >0 表示 a 更新，<0 表示 b 更新，0 相等。
 */
export function compareVersions(a: string, b: string): number {
  const norm = (v: string) => v.replace(/^v/i, '');
  const pa = norm(a).split(/[.\-+]/).map((x) => Number.parseInt(x, 10) || 0);
  const pb = norm(b).split(/[.\-+]/).map((x) => Number.parseInt(x, 10) || 0);
  const len = Math.max(pa.length, pb.length);
  for (let i = 0; i < len; i++) {
    const x = pa[i] ?? 0;
    const y = pb[i] ?? 0;
    if (x !== y) return x - y;
  }
  return 0;
}
