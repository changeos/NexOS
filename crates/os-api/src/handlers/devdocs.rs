//! `DevDocsRouteHandler` —— 「开发者中心」桌面应用的 HTTP 适配器：
//! 仓库 `docs/` 目录（文档唯一事实源）的只读索引与原文服务。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/devdocs/*`）翻译为对文档目录的扫描 /
//! 读取，返回 JSON。这是 OS「开发者中心」桌面应用的后端 REST 入口。
//!
//! # 架构：文档唯一事实源 = 仓库 `docs/`
//!
//! 开发者中心是**渲染与服务层**，不含文档本体——文档随仓库演进
//! （受仓库「功能文档同步铁律」约束：每个功能的新增能力必须在该功能的
//! MD 里说明），`git push` 即更新（NexHub post-receive 钩子已自动化），
//! 因此「不断更新的文档门户」零额外机制：
//!
//! ```text
//! 仓库 docs/*.md（唯一事实源，随代码演进）
//!    │ git push（钩子已自动化）
//!    ▼
//! os-api devdocs handler（读 NEXOS_DEVDOCS_DIR 目录）
//!    ▼
//! 桌面应用「开发者中心」（文档目录 + Markdown 渲染 + 搜索）
//! ```
//!
//! # 索引（GET /index）
//!
//! 扫描文档根目录的 `*.md`（含**一级子目录**，如 `dev/`、`adr/`、`agents/`），
//! 每篇提取：标题（首个 `# ` 行）+ 相对路径 + 字节数 + mtime + 分类
//! （frontmatter `category:` 优先，否则按一级子目录名，根目录文件为 `docs`）。
//! 结果缓存 30s（目录 mtime 变化立即失效——新增/删除文档即时可见）。
//!
//! # 原文（GET /doc/*path）
//!
//! 返回 `{path, title, markdown, mtime}`——markdown 原文由前端渲染
//! （marked），本服务不做 HTML 转换。路径安全三闸：
//!
//! 1. 拒绝非 `.md` 后缀（Cargo.toml / 二进制等一律 400）；
//! 2. 拼接后 `canonicalize`，解析结果必须仍在文档根内（`..` 穿越 / 符号
//!    链接出根一律 403）；
//! 3. 文件不存在 → 404。
//!
//! # AI 翻译（GET /doc/*path?lang=en|zh-TW，本地 LLM 管线）
//!
//! 「吃自己的狗粮」：文档全中文，目标语言 v1 支持 `en` / `zh-TW`（缺省或
//! `lang=zh` 直读原文零开销）。管线（未命中 → 任务 → 缓存）：
//!
//! ```text
//! GET /doc/x.md?lang=en
//!    │ ① 缓存 /tank/os-data/devdocs-i18n/en/x.md 命中且未过期（原文 mtime
//!    │    不新于译文 mtime）→ 200 译文 + 响应头 X-Translation: cached
//!    ▼ ② miss → 异步翻译任务（内存表，环形日志 200 行）
//! GET /devdocs/translate/tasks/:id 轮询（202 响应体即任务视图）
//!    │    逐块经本节点 API 网关 chat/completions（服务端凭据，构造期定格）
//!    ▼ ③ 完成 → 原子写缓存 → 再取 ?lang= 即 200 译文
//! ```
//!
//! - **分块**（16K 上下文约束）：按二级标题分节，每块 ≤6K 字符，超长节再按
//!   空行段落累积切（段落本身超长按行硬切）；frontmatter（`---` 元数据块）
//!   不翻译原样回接。fence（```/~~~）内的 `##`/空行不作为切分边界。
//! - **prompt 契约**：技术文档翻译；代码块/命令/URL/路径/mermaid/ASCII 图/
//!   表格结构原样保留；术语表（NodeID/overlay/ZFS/vLLM 等，见 [`GLOSSARY`]）
//!   不译；Markdown 结构不变。
//! - **思考模型适配**（106 真机验证，2026-09-03）：请求体带 vLLM Qwen3 官方
//!   开关 `chat_template_kwargs.enable_thinking=false`（生效）；content 空且
//!   reasoning 出现/finish=length → 判思考占用，`/no_think` 软开关重试一次
//!   （该后端实测无效，保留作降级通道）仍空才 error；max_tokens 动态 =
//!   输入字符/2 + 2048。详见 DEVDOCS_DEV_CENTER.md §5.3。
//! - **凭据**：env `NEXOS_DEVDOCS_GATEWAY_TOKEN`（sk-os- 网关令牌，优先）→
//!   回落 `NEXOS_ADMIN_TOKEN`/`OS_ADMIN_TOKEN`（构造期定格，llm.rs 不碰）。
//! - **诚实降级**：网关无渠道/无可用模型（404/502）→ 任务 error，后续
//!   `?lang=` 请求 503 + 文案「本节点无可用本地模型…（中文原文可用）」——
//!   不假翻译。`retry=1` 可清除失败态重试。
//! - **失效**（v1 简化）：原文 mtime 新于译文 mtime → 缓存判 miss 直接重译
//!   （旧译不返回），见 [`translation_cache_fresh`]。
//! - **联邦**：无 checkout 的联邦节点 doc 请求把 `lang` 透传源节点（译文由
//!   源节点缓存/翻译；源节点 202 的任务 id 不在本节点任务表——前端对任务
//!   404 回退为定时重取原文）。
//!
//! # 降级与联邦回退（无 checkout 的部署节点）
//!
//! 文档根解析顺序：env `NEXOS_DEVDOCS_DIR` → 缺省 `/home/oem/NexOS/docs`
//! （106 主节点 checkout）→ 二进制旁 `./docs` → 二进制旁 `../../docs`
//! （workspace 内 target/{debug,release} 运行形态）。全部不存在时进入
//! **降级模式**；若 env `NEXOS_DEVDOCS_FALLBACK_URL` 配置了联邦源节点
//! （113/aliyun 等无 checkout 节点指向 `http://192.0.2.106:8558`），
//! 则升级为**联邦回退**——从源节点代理拉取，不再显示空目录：
//!
//! - index：GET `{fallback}/api/v1/devdocs/index`（10s 超时）原样透传 JSON，
//!   仅 `note` 覆写为「联邦文档分发：<源节点>」；透传结果缓存 30s（同本地），
//!   拉取失败落回本地降级（空清单 + 提示），失败不缓存（下次重试）。
//! - doc：GET `{fallback}/api/v1/devdocs/doc/<path>` 状态码与 JSON 原样透传
//!   （不缓存；防穿越主责在源节点，本地仍先拒含 `..` 的路径）。
//!
//! 未配置联邦源时保持纯降级：index 空清单 + `source_available:false` + 提示
//! 文案（「文档服务在本仓库节点」），doc 503——节点不 crash 不报 500。
//!
//! # 鉴权
//!
//! 开发期全部公开读（requires_auth=false，跟随 agent_coord 等开发期惯例）。
//!
//! # 路由表（3 条，component="devdocs"）
//!
//! | method | path                                        | 动作 |
//! |--------|---------------------------------------------|------|
//! | GET    | `/api/v1/devdocs/index`                     | 文档索引（分类/标题/路径/大小/mtime，缓存 30s）|
//! | GET    | `/api/v1/devdocs/doc/*path`                 | 单篇原文 `{path, title, markdown, mtime}`；`?lang=en\|zh-TW` 走翻译管线（200 译文 / 202 任务 / 503 降级） |
//! | GET    | `/api/v1/devdocs/translate/tasks/:id`       | 翻译任务视图（状态机 + 环形日志，轮询用） |

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// 常量
// ----------------------------------------------------------------------------

/// 组件名（路由注册用）。
const COMPONENT: &str = "devdocs";

/// 文档根缺省路径（106 主节点 checkout；env `NEXOS_DEVDOCS_DIR` 覆盖）。
pub const DEFAULT_DEVDOCS_DIR: &str = "/home/oem/NexOS/docs";

/// 索引缓存 TTL（目录 mtime 无变化时的最长复用时间）。
const CACHE_TTL: Duration = Duration::from_secs(30);

/// 降级提示（无 checkout 且未配置联邦源的节点的前端展示文案）。
const DEGRADED_NOTE: &str =
    "文档服务在本仓库节点（docs/ 随 git push 更新；本节点未检出仓库，可配置 \
     NEXOS_DEVDOCS_FALLBACK_URL 指向仓库节点启用联邦回退）";

/// 联邦回退源节点 env（base URL，如 `http://192.0.2.106:8558`）。
const FALLBACK_ENV: &str = "NEXOS_DEVDOCS_FALLBACK_URL";

/// 联邦回退单次 HTTP 超时。
const FALLBACK_TIMEOUT: Duration = Duration::from_secs(10);

/// 联邦回退共享 `reqwest::Client`（连接池复用，api_gateway HTTP 同款手法；
/// 10s 兜底超时，各调用处不再单独覆盖）。
static FALLBACK_HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(FALLBACK_TIMEOUT)
        .build()
        .expect("构建 devdocs 联邦回退 reqwest Client 失败")
});

// ----------------------------------------------------------------------------
// AI 翻译管线常量（本地 LLM，经本节点 API 网关；设计定稿 v1）
// ----------------------------------------------------------------------------

/// 翻译缓存根缺省（env `NEXOS_DEVDOCS_I18N_DIR` 覆盖；106 的 /tank 数据盘）。
pub const DEFAULT_I18N_DIR: &str = "/tank/os-data/devdocs-i18n";

/// 翻译缓存根覆盖 env。
const I18N_DIR_ENV: &str = "NEXOS_DEVDOCS_I18N_DIR";

/// 翻译走本节点 API 网关的 base URL 覆盖 env。
const GATEWAY_URL_ENV: &str = "NEXOS_DEVDOCS_GATEWAY_URL";

/// 网关 base URL 缺省（os-api 缺省端口，provisioning 同款）。
const DEFAULT_GATEWAY_URL: &str = "http://127.0.0.1:8558";

/// 网关服务端凭据 env（sk-os- 网关令牌；优先于 admin 回落——网关
/// chat/completions 鉴权查令牌表，admin token 仅在运维把它注册为网关令牌时可用）。
const GATEWAY_TOKEN_ENV: &str = "NEXOS_DEVDOCS_GATEWAY_TOKEN";

/// 翻译模型覆盖 env（网关渠道对外模型名；缺省取 106 现网 Qwen3.5 渠道模型名）。
const TRANSLATE_MODEL_ENV: &str = "NEXOS_DEVDOCS_TRANSLATE_MODEL";

/// 翻译模型缺省（106 现网网关渠道 ch-101「Qwen3.5-9B」的对外模型名）。
const DEFAULT_TRANSLATE_MODEL: &str = "qwen3.5-9b";

/// 单块字符上限（16K 上下文约束下留足 prompt+译文余量）。
const CHUNK_MAX_CHARS: usize = 6000;

/// 单块网关调用超时。对齐网关代理转发的 300s 上限（2026-09-03 起网关
/// 代理超时 60s→300s）：客户端 ≥ 网关，先到的是网关的明确错误而不是本地掐断。
const CHUNK_TIMEOUT: Duration = Duration::from_secs(300);

/// 任务日志环形上限（行）。
const TASK_LOG_MAX_LINES: usize = 200;

/// 任务表容量上限（超过丢弃最旧的已完结任务，running 永不丢）。
const TASK_KEEP_MAX: usize = 128;

/// 并发翻译任务上限（本地模型带宽有限，超出 503 请稍后重试）。
const MAX_CONCURRENT_TRANSLATIONS: usize = 4;

/// 任务视为挂死的兜底时长（GET tasks/:id 时惰性判超时并转 error）。
const TASK_STALE_AFTER: i64 = 45 * 60;

/// 术语表（prompt 中声明不译；增补即改这里）。
const GLOSSARY: &[&str] = &[
    "NodeID",
    "OverlayAddr",
    "overlay",
    "ZFS",
    "vLLM",
    "NexOS",
    "NexHub",
    "os-api",
    "os-common",
    "os-security",
    "axum",
    "Vue",
    "Rust",
    "cargo",
    "crates",
    "JWT",
    "API",
    "SDK",
    "SSE",
    "WebSocket",
    "REST",
    "HTTP",
    "JSON",
    "SQLite",
    "systemd",
    "Docker",
    "Markdown",
    "frontmatter",
    "post-receive",
    "GLM",
    "Qwen",
    "USDT",
    "EVM",
    "secp256k1",
    "keccak",
];

/// 翻译专用共享 `reqwest::Client`：不设总超时（单块可能分钟级），仅 10s
/// 连接建立超时；每请求再按 [`CHUNK_TIMEOUT`] 覆盖。
static TRANSLATE_HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()
        .expect("构建 devdocs 翻译 reqwest Client 失败")
});

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 一篇文档的索引项（GET /index 列表元素）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocEntry {
    /// 相对文档根的路径（URL 直用，如 `dev/01-app-development.md`）
    pub path: String,
    /// 标题（正文首个 `# ` 行；无则回退文件名）
    pub title: String,
    /// 分类：frontmatter `category:` > 一级子目录名 > `docs`
    pub category: String,
    /// 字节数
    pub size: u64,
    /// 最后修改时间（ISO；未知为 null）
    pub mtime: Option<String>,
}

/// GET /index 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexResp {
    /// 索引项（按 category + path 排序）
    pub docs: Vec<DocEntry>,
    /// 分类名列表（出现顺序，供前端目录树分组）
    pub categories: Vec<String>,
    /// 文档根是否可用（false = 降级模式，见模块文档）
    pub source_available: bool,
    /// 实际使用的文档根路径（降级时为解析失败前的候选）
    pub root: String,
    /// 降级提示（source_available=false 时非空）
    pub note: Option<String>,
}

/// GET /doc/*path 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocResp {
    /// 相对路径（回显）
    pub path: String,
    /// 标题（首个 `# ` 行）
    pub title: String,
    /// Markdown 原文（前端渲染）
    pub markdown: String,
    /// 最后修改时间（ISO；未知为 null）
    pub mtime: Option<String>,
}

/// 翻译任务视图（202 响应体 / GET /translate/tasks/:id 响应）。
#[derive(Debug, Clone, Serialize)]
pub struct TranslateTaskView {
    pub id: String,
    /// 目标语言目录名（`en` / `zh-TW`）
    pub lang: String,
    /// 文档相对路径（缓存键的一部分）
    pub path: String,
    /// `running` / `done` / `error`
    pub status: String,
    /// 分块总数（请求时即已知——分块是纯函数）
    pub chunks_total: usize,
    /// 已完成块数（进度轮询展示）
    pub chunks_done: usize,
    /// 环形日志（上限 [`TASK_LOG_MAX_LINES`] 行）
    pub log: Vec<String>,
    /// 开始时刻（epoch 秒）
    pub started_at: i64,
    /// 结束时刻（epoch 秒；未结束 null）
    pub finished_at: Option<i64>,
    /// 失败原因（status=error 时非空；含降级文案）
    pub error: Option<String>,
}

// ----------------------------------------------------------------------------
// AI 翻译：目标语言
// ----------------------------------------------------------------------------

/// 目标语言（`?lang=` 解析结果；缺省/zh = 原文直读，不在本枚举内）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetLang {
    /// 英文。
    En,
    /// 繁体中文。
    ZhTw,
}

impl TargetLang {
    /// 解析 `?lang=` 值：`None` = 缺省/zh（原文直读）；`Err` = 不支持的语言。
    fn parse(value: Option<&str>) -> Result<Option<TargetLang>, String> {
        let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) else {
            return Ok(None);
        };
        match v.to_ascii_lowercase().as_str() {
            "zh" | "zh-cn" | "zh-hans" => Ok(None),
            "en" | "en-us" => Ok(Some(TargetLang::En)),
            "zh-tw" | "zh-hant" => Ok(Some(TargetLang::ZhTw)),
            other => Err(format!(
                "不支持的语言「{other}」（v1 支持 en / zh-TW；中文原文无需 lang 参数）"
            )),
        }
    }

    /// 缓存目录名（同时是任务键的一部分）。
    fn dir_name(&self) -> &'static str {
        match self {
            TargetLang::En => "en",
            TargetLang::ZhTw => "zh-TW",
        }
    }

    /// 降级文案里的语言称呼。
    fn display(&self) -> &'static str {
        match self {
            TargetLang::En => "English",
            TargetLang::ZhTw => "繁體中文",
        }
    }

    /// prompt 里的目标语言说明。
    fn prompt_target(&self) -> &'static str {
        match self {
            TargetLang::En => "English（英文，技术文档惯用表述）",
            TargetLang::ZhTw => "繁體中文（台湾惯用技术译法，注意用词习惯与简体的差异）",
        }
    }
}

/// 无可用本地模型的诚实降级文案（设计定稿原文）。
fn no_model_msg(lang: TargetLang) -> String {
    format!(
        "本节点无可用本地模型，暂无法生成 {} 翻译（中文原文可用）",
        lang.display()
    )
}

// ----------------------------------------------------------------------------
// 缓存条目
// ----------------------------------------------------------------------------

/// 索引缓存：一次真实扫描的结果 + 根目录 mtime 快照。
#[derive(Debug, Clone)]
struct CacheEntry {
    fetched_at: Instant,
    root_mtime: Option<SystemTime>,
    resp: IndexResp,
}

/// 联邦回退 index 缓存：源节点透传 JSON + 抓取时刻（TTL 同本地 30s；无
/// mtime 失效通道——源节点内容新鲜度由源节点自身索引缓存与文档更新节奏决定）。
#[derive(Debug, Clone)]
struct FallbackCacheEntry {
    fetched_at: Instant,
    body: serde_json::Value,
}

// ----------------------------------------------------------------------------
// AI 翻译：任务态（进程内内存表 + 环形日志，agenthub_toolchain 同款手法）
// ----------------------------------------------------------------------------

/// 翻译任务（进程内态；重启即清，译文在磁盘缓存上）。
#[derive(Debug, Clone)]
struct TranslateTask {
    id: String,
    lang: String,
    path: String,
    /// `running` / `done` / `error`
    status: String,
    chunks_total: usize,
    chunks_done: usize,
    log: Vec<String>,
    started_at: i64,
    finished_at: Option<i64>,
    error: Option<String>,
}

impl From<&TranslateTask> for TranslateTaskView {
    fn from(t: &TranslateTask) -> Self {
        TranslateTaskView {
            id: t.id.clone(),
            lang: t.lang.clone(),
            path: t.path.clone(),
            status: t.status.clone(),
            chunks_total: t.chunks_total,
            chunks_done: t.chunks_done,
            log: t.log.clone(),
            started_at: t.started_at,
            finished_at: t.finished_at,
            error: t.error.clone(),
        }
    }
}

/// 翻译任务注册表：id → 任务 + (lang, path) → 最新任务 id。
struct TranslateRegistry {
    tasks: Mutex<HashMap<String, TranslateTask>>,
    by_key: Mutex<HashMap<(String, String), String>>,
    seq: AtomicU64,
}

impl TranslateRegistry {
    fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            by_key: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(0),
        }
    }

    /// 任务快照（无任务 / 任务已被 GC → None）。
    fn snapshot(&self, id: &str) -> Option<TranslateTaskView> {
        let tasks = self.tasks.lock().expect("devdocs translate tasks poisoned");
        tasks.get(id).map(TranslateTaskView::from)
    }

    /// (lang, path) 最新任务快照。
    fn latest(&self, key: &(String, String)) -> Option<TranslateTaskView> {
        let id = self
            .by_key
            .lock()
            .expect("devdocs translate by_key poisoned")
            .get(key)
            .cloned()?;
        self.snapshot(&id)
    }

    /// running 任务数（并发闸）。
    fn running_count(&self) -> usize {
        self.tasks
            .lock()
            .expect("devdocs translate tasks poisoned")
            .values()
            .filter(|t| t.status == "running")
            .count()
    }

    /// 登记任务（running 态）并返回 id；顺手 GC 超容量的最旧已完结任务。
    fn register(&self, lang: &str, path: &str, chunks_total: usize) -> String {
        let id = format!(
            "ddt-{}",
            self.seq.fetch_add(1, Ordering::SeqCst) + 1
        );
        let mut tasks = self.tasks.lock().expect("devdocs translate tasks poisoned");
        if tasks.len() >= TASK_KEEP_MAX {
            // 丢最旧的已完结任务（按 started_at；running 永不丢）。
            let mut finished: Vec<(i64, String)> = tasks
                .iter()
                .filter(|(_, t)| t.status != "running")
                .map(|(id, t)| (t.started_at, id.clone()))
                .collect();
            finished.sort();
            let overflow = tasks.len() + 1 - TASK_KEEP_MAX;
            for (_, old) in finished.into_iter().take(overflow) {
                tasks.remove(&old);
            }
        }
        tasks.insert(
            id.clone(),
            TranslateTask {
                id: id.clone(),
                lang: lang.to_string(),
                path: path.to_string(),
                status: "running".into(),
                chunks_total,
                chunks_done: 0,
                log: Vec::new(),
                started_at: now_epoch(),
                finished_at: None,
                error: None,
            },
        );
        drop(tasks);
        self.by_key
            .lock()
            .expect("devdocs translate by_key poisoned")
            .insert((lang.to_string(), path.to_string()), id.clone());
        id
    }

    /// 任务日志追加一行（环形上限；后台翻译线程与请求线程共用）。
    fn log(&self, id: &str, line: &str) {
        let mut tasks = self.tasks.lock().expect("devdocs translate tasks poisoned");
        if let Some(t) = tasks.get_mut(id) {
            t.log.push(line.to_string());
            if t.log.len() > TASK_LOG_MAX_LINES {
                let cut = t.log.len() - TASK_LOG_MAX_LINES;
                t.log.drain(0..cut);
            }
        }
    }

    /// 推进块进度。
    fn chunk_done(&self, id: &str) {
        let mut tasks = self.tasks.lock().expect("devdocs translate tasks poisoned");
        if let Some(t) = tasks.get_mut(id) {
            t.chunks_done += 1;
        }
    }

    /// 任务收尾（done / error + finished_at + 收尾日志行）。
    fn finish(&self, id: &str, status: &str, line: &str, error: Option<String>) {
        self.log(id, line);
        let mut tasks = self.tasks.lock().expect("devdocs translate tasks poisoned");
        if let Some(t) = tasks.get_mut(id) {
            t.status = status.to_string();
            t.finished_at = Some(now_epoch());
            t.error = error;
        }
    }

    /// 惰性超时：running 超过 [`TASK_STALE_AFTER`] 秒（后台任务线程异常死亡）
    /// → 就地转 error，前端轮询能拿到终态而不是永远 running。
    fn reap_stale(&self, id: &str) {
        let stale = {
            let tasks = self.tasks.lock().expect("devdocs translate tasks poisoned");
            tasks.get(id).is_some_and(|t| {
                t.status == "running" && now_epoch() - t.started_at > TASK_STALE_AFTER
            })
        };
        if stale {
            self.finish(
                id,
                "error",
                &format!("✘ 任务超时（>{TASK_STALE_AFTER}s），判定失败"),
                Some(format!("翻译任务超时（>{TASK_STALE_AFTER}s）")),
            );
        }
    }
}

/// 翻译管线配置（构造期定格；`None` gateway_token = 未配置凭据，请求时 503）。
struct I18nConfig {
    /// 译文缓存根（`<dir>/<lang>/<相对路径>`；构造期已验证可创建）。
    cache_dir: PathBuf,
    /// 本节点 API 网关 base URL。
    gateway_url: String,
    /// 网关服务端凭据（sk-os- 优先，admin 回落）。
    gateway_token: Option<String>,
    /// 网关渠道对外模型名。
    model: String,
}

// ----------------------------------------------------------------------------
// Handler
// ----------------------------------------------------------------------------

/// 「开发者中心」REST 适配器（模块文档见顶部）。
pub struct DevDocsRouteHandler {
    /// 文档根（None = 降级模式：目录不存在，见模块文档）。
    root: Option<PathBuf>,
    /// 联邦回退源节点 base URL（None = 未配置；仅本地无文档根时启用）。
    fallback: Option<String>,
    /// 索引缓存（30s / 目录 mtime 失效）。
    cache: Mutex<Option<CacheEntry>>,
    /// 联邦回退 index 透传缓存（30s TTL；doc 不缓存）。
    fallback_cache: Mutex<Option<FallbackCacheEntry>>,
    /// AI 翻译配置（None = 缓存根不可创建，lang 请求一律 503）。
    i18n: Option<I18nConfig>,
    /// 翻译任务注册表（进程内内存表 + 环形日志）。
    translate: Arc<TranslateRegistry>,
    /// 测试可观测：真实扫描次数（缓存命中不增加）。
    #[cfg(test)]
    scan_count: std::sync::atomic::AtomicUsize,
}

impl DevDocsRouteHandler {
    /// 生产构造：解析文档根（env → 缺省 → 二进制旁回退，见模块文档）、
    /// 联邦回退源（env `NEXOS_DEVDOCS_FALLBACK_URL`）与 AI 翻译配置
    /// （env 见 [`I18nConfig`] 各字段）。
    pub fn new() -> Self {
        Self::with_root_and_fallback(resolve_root(), fallback_from_env())
    }

    /// 注入文档根（测试用；None 构造降级模式，联邦回退与翻译不启用——保持
    /// 既有降级行为断言的确定性）。
    #[must_use]
    pub fn with_root(root: Option<PathBuf>) -> Self {
        Self::with_root_and_fallback(root, None)
    }

    /// 注入文档根 + 联邦回退源（测试用；`fallback` 为源节点 base URL）。
    #[must_use]
    pub fn with_root_and_fallback(root: Option<PathBuf>, fallback: Option<String>) -> Self {
        Self::build(
            root,
            fallback,
            // 测试构造不读翻译 env：默认目录 /tank 在测试机上可能不存在，
            // 且 mock 网关地址由专用构造注入，避免测试互相污染。
            None,
        )
    }

    /// 注入文档根 + 完整翻译配置（翻译链路测试用；模块私有——仅本模块测试
    /// 构造，`i18n.cache_dir` 须已存在或可创建——本构造不验证，测试自行保证）。
    #[cfg(test)]
    #[must_use]
    fn with_root_and_i18n(root: Option<PathBuf>, i18n: I18nConfig) -> Self {
        Self::build(root, None, Some(i18n))
    }

    /// 唯一真实构造体。
    fn build(
        root: Option<PathBuf>,
        fallback: Option<String>,
        i18n: Option<I18nConfig>,
    ) -> Self {
        Self {
            root: root.filter(|p| p.is_dir()),
            fallback: fallback
                .map(|u| u.trim().trim_end_matches('/').to_string())
                .filter(|u| !u.is_empty()),
            cache: Mutex::new(None),
            fallback_cache: Mutex::new(None),
            i18n: i18n.or_else(resolve_i18n_config),
            translate: Arc::new(TranslateRegistry::new()),
            #[cfg(test)]
            scan_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// 实际文档根（降级时 None；client.ts 的 root 展示与测试用）。
    pub fn root_path(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// 索引（带缓存）：缓存新鲜（<30s 且根目录 mtime 未变）→ 直接复用；
    /// 否则真实扫描并写回缓存。
    fn index(&self) -> IndexResp {
        // 降级模式：无缓存必要，直接回空清单 + 提示。
        let Some(root) = self.root.as_ref() else {
            return IndexResp {
                docs: vec![],
                categories: vec![],
                source_available: false,
                root: DEFAULT_DEVDOCS_DIR.to_string(),
                note: Some(DEGRADED_NOTE.to_string()),
            };
        };
        let root_mtime = dir_mtime(root);
        {
            let cache = self.cache.lock().expect("devdocs cache poisoned");
            if let Some(entry) = cache.as_ref() {
                if entry.fetched_at.elapsed() < CACHE_TTL && entry.root_mtime == root_mtime {
                    return entry.resp.clone();
                }
            }
        }
        let resp = self.scan(root);
        *self.cache.lock().expect("devdocs cache poisoned") = Some(CacheEntry {
            fetched_at: Instant::now(),
            root_mtime,
            resp: resp.clone(),
        });
        resp
    }

    /// 真实扫描：根目录 `*.md` + 一级子目录 `*.md`（不递归更深——adr/agents
    /// 自身有子目录的取其一；两层足够目录树分组，避免深仓库全量噪声）。
    fn scan(&self, root: &Path) -> IndexResp {
        #[cfg(test)]
        self.scan_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let mut docs: Vec<DocEntry> = Vec::new();
        // 候选目录：根 + 一级子目录（仅目录，排序保证输出稳定）。
        let mut dirs: Vec<PathBuf> = vec![root.to_path_buf()];
        if let Ok(rd) = std::fs::read_dir(root) {
            let mut subs: Vec<PathBuf> = rd
                .flatten()
                .filter(|e| e.path().is_dir())
                .map(|e| e.path())
                .collect();
            subs.sort();
            dirs.extend(subs);
        }
        for dir in dirs {
            let rel_prefix = dir
                .strip_prefix(root)
                .map(|p| {
                    let s = p.to_string_lossy().replace('\\', "/");
                    if s.is_empty() {
                        String::new()
                    } else {
                        format!("{s}/")
                    }
                })
                .unwrap_or_default();
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            let mut files: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "md"))
                .collect();
            files.sort();
            for f in files {
                let name = f
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let path = format!("{rel_prefix}{name}");
                let meta = std::fs::metadata(&f).ok();
                let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                let mtime = meta
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .and_then(system_time_to_iso);
                let (title, category) = read_title_and_category(&path, &f);
                docs.push(DocEntry {
                    path,
                    title,
                    category,
                    size,
                    mtime,
                });
            }
        }
        docs.sort_by(|a, b| {
            a.category
                .cmp(&b.category)
                .then_with(|| a.path.cmp(&b.path))
        });
        // 分类出现顺序（docs 已按 category 排序，去重保序即分组序）。
        let categories = docs
            .iter()
            .map(|d| d.category.clone())
            .fold(Vec::new(), |mut acc, c| {
                if !acc.contains(&c) {
                    acc.push(c);
                }
                acc
            });
        IndexResp {
            root: root.to_string_lossy().replace('\\', "/"),
            docs,
            categories,
            source_available: true,
            note: None,
        }
    }

    /// 读单篇原文（路径安全三闸见模块文档）；额外返回 canonical 绝对路径
    /// （翻译流程推导缓存相对键用——`rel` 原串可能含已规整的 `a/../b` 形态，
    /// 直接拼缓存路径有越界风险，canonical 前缀剥离后才是安全键）。
    fn read_doc(&self, rel: &str) -> Result<(DocResp, PathBuf), (u16, String)> {
        let Some(root) = self.root.as_ref() else {
            return Err((503, DEGRADED_NOTE.to_string()));
        };
        // 闸 1：仅 .md（拒绝 Cargo.toml / 脚本 / 二进制）。
        if !rel.to_ascii_lowercase().ends_with(".md") {
            return Err((400, "仅支持 .md 文档".into()));
        }
        // 闸 2：拼接 + canonicalize 后必须仍在根内（防 `..` 穿越 / 符号链接出根）。
        let target = root.join(rel);
        let Ok(canonical) = target.canonicalize() else {
            return Err((404, format!("文档不存在: {rel}")));
        };
        let Ok(root_canonical) = root.canonicalize() else {
            return Err((500, "文档根不可用".into()));
        };
        if !canonical.starts_with(&root_canonical) || !canonical.is_file() {
            return Err((403, "路径越界（仅允许文档根内）".into()));
        }
        let markdown =
            std::fs::read_to_string(&canonical).map_err(|e| (500, format!("读取失败: {e}")))?;
        let mtime = std::fs::metadata(&canonical)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(system_time_to_iso);
        let (title, _) = extract_title_and_category(&markdown, rel);
        Ok((
            DocResp {
                path: rel.to_string(),
                title,
                markdown,
                mtime,
            },
            canonical,
        ))
    }

    // ---- 联邦回退（本地无文档根 + env 配置源节点时启用，见模块文档）----

    /// 联邦回退 index：从源节点代理拉取（透传 JSON，`note` 覆写为来源提示；
    /// 30s 缓存）。返回 `None` = 不走联邦（本地有根 / 未配置 / 拉取失败——
    /// 调用方落回本地降级响应）。
    async fn federated_index(&self) -> Option<ApiResponse> {
        if self.root.is_some() {
            return None;
        }
        let base = self.fallback.as_deref()?;
        // 缓存命中（TTL 30s，同本地索引）。
        {
            let cache = self
                .fallback_cache
                .lock()
                .expect("devdocs fallback cache poisoned");
            if let Some(e) = cache.as_ref() {
                if e.fetched_at.elapsed() < CACHE_TTL {
                    return Some(ok_json(e.body.clone()));
                }
            }
        }
        let url = format!("{base}/api/v1/devdocs/index");
        let resp = FALLBACK_HTTP.get(&url).send().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let mut body: serde_json::Value = resp.json().await.ok()?;
        body["note"] = serde_json::Value::String(format!("联邦文档分发：{base}"));
        *self
            .fallback_cache
            .lock()
            .expect("devdocs fallback cache poisoned") = Some(FallbackCacheEntry {
            fetched_at: Instant::now(),
            body: body.clone(),
        });
        Some(ok_json(body))
    }

    /// 联邦回退读单篇：代理 GET `{fallback}/api/v1/devdocs/doc/<rel>`，
    /// 状态码与 JSON 原样透传（不缓存）；`lang` 查询参数透传源节点（译文由
    /// 源节点缓存/翻译——源节点 202 的任务 id 不在本节点任务表，前端对任务
    /// 404 回退定时重取）；源不可达 → 503 说明。
    async fn federated_doc(&self, rel: &str, lang: Option<&str>) -> ApiResponse {
        let base = self
            .fallback
            .as_deref()
            .expect("federated_doc 仅在已配置联邦源时调用");
        let mut url = format!("{base}/api/v1/devdocs/doc/{rel}");
        if let Some(lang) = lang.filter(|l| !l.trim().is_empty()) {
            url.push_str("?lang=");
            url.push_str(&percent_encode_query(lang.trim()));
        }
        match FALLBACK_HTTP.get(&url).send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let body = resp
                    .json::<serde_json::Value>()
                    .await
                    .unwrap_or_else(|_| serde_json::json!({"error": "联邦源响应非 JSON"}));
                ApiResponse {
                    status,
                    body,
                    headers: serde_json::json!({}),
                }
            }
            Err(e) => error_response(
                503,
                &format!("联邦文档源不可达（{base}）：{e}；{DEGRADED_NOTE}"),
            ),
        }
    }

    // ---- AI 翻译（GET /doc/*path?lang=，流程见模块文档）----

    /// `?lang=<目标>` 取文档：缓存命中 → 200 译文（X-Translation: cached）；
    /// miss → 任务状态机（running 202 / error 503 / 新任务 202）。`retry=1`
    /// 清除失败态强制重试。
    async fn doc_with_lang(&self, rel: &str, lang: TargetLang, retry: bool) -> ApiResponse {
        let Some(i18n) = self.i18n.as_ref() else {
            return error_response(
                503,
                "翻译功能不可用：译文缓存目录无法创建（检查 NEXOS_DEVDOCS_I18N_DIR 指向的路径及其权限）",
            );
        };
        // 三闸 + 读原文（复用原文路径——翻译失败时调用方仍可读中文原文）。
        let (doc, canonical) = match self.read_doc(rel) {
            Ok(v) => v,
            Err((status, msg)) => return error_response(status, &msg),
        };
        let root_canonical = self
            .root
            .as_ref()
            .and_then(|r| r.canonicalize().ok())
            .unwrap_or_else(|| canonical.clone());
        let safe_rel = canonical
            .strip_prefix(&root_canonical)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| rel.replace('\\', "/"));
        let source_mtime = std::fs::metadata(&canonical)
            .ok()
            .and_then(|m| m.modified().ok());
        let cache_path = i18n.cache_dir.join(lang.dir_name()).join(&safe_rel);

        // ① 缓存命中且未过期（原文 mtime 不新于译文 mtime——v1 简化：过期即
        //    miss 重译，旧译不返回，见模块文档「失效」）。
        if let Some(translated) = translation_cache_fresh(&cache_path, source_mtime) {
            let (title, _) = extract_title_and_category(&translated, rel);
            return ApiResponse {
                status: 200,
                body: serde_json::json!({
                    "path": rel,
                    "title": title,
                    "markdown": translated,
                    "mtime": doc.mtime,
                }),
                headers: serde_json::json!({ "X-Translation": "cached" }),
            };
        }

        // ② 已有任务：running → 202 复用；error（且未要求重试）→ 503 降级文案。
        let key = (lang.dir_name().to_string(), safe_rel.clone());
        if !retry {
            if let Some(view) = self.translate.latest(&key) {
                match view.status.as_str() {
                    "running" => return task_accepted(view),
                    "error" => {
                        return ApiResponse {
                            status: 503,
                            body: serde_json::json!({
                                "error": view.error.clone().unwrap_or_else(|| "翻译失败".into()),
                                "task": view,
                            }),
                            headers: serde_json::json!({}),
                        };
                    }
                    _ => {} // done 但缓存仍 miss（翻译期间原文又更新）→ 落到重译
                }
            }
        }

        // ③ 新任务：凭据 / 并发闸 → spawn 后台逐块翻译。
        let Some(token) = i18n.gateway_token.clone().filter(|t| !t.trim().is_empty()) else {
            return error_response(
                503,
                &format!(
                    "{}；未配置翻译服务端凭据：设置 NEXOS_DEVDOCS_GATEWAY_TOKEN（sk-os- 网关令牌，\
                     优先）或 NEXOS_ADMIN_TOKEN（须同时是网关令牌表中的 key）后重启",
                    no_model_msg(lang)
                ),
            );
        };
        if self.translate.running_count() >= MAX_CONCURRENT_TRANSLATIONS {
            return error_response(
                503,
                &format!(
                    "翻译任务并发已满（最多 {MAX_CONCURRENT_TRANSLATIONS} 篇同时翻译，本地模型带宽有限），请稍后重试"
                ),
            );
        }
        let (frontmatter, chunks) = split_translation_chunks(&doc.markdown, CHUNK_MAX_CHARS);
        let task_id = self
            .translate
            .register(lang.dir_name(), &safe_rel, chunks.len().max(1));
        self.translate.log(
            &task_id,
            &format!(
                "▶ 翻译 {} → {}（{} 块，每块 ≤{CHUNK_MAX_CHARS} 字符；模型 {} @ {}）",
                safe_rel,
                lang.display(),
                chunks.len(),
                i18n.model,
                i18n.gateway_url
            ),
        );
        eprintln!(
            "[devdocs] 翻译任务启动：{task_id}（{safe_rel} → {}，{} 块）",
            lang.dir_name(),
            chunks.len()
        );
        let job = TranslateJob {
            task_id: task_id.clone(),
            lang,
            path: safe_rel,
            frontmatter,
            chunks,
            cache_path,
            gateway_url: i18n.gateway_url.clone(),
            gateway_token: token,
            model: i18n.model.clone(),
            registry: Arc::clone(&self.translate),
        };
        tokio::spawn(run_translation(job));
        task_accepted(self.translate.snapshot(&task_id).expect("刚登记的任务必然存在"))
    }
}

impl Default for DevDocsRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

// ----------------------------------------------------------------------------
// 纯函数：标题/分类提取 / 目录 mtime / 文档根解析 / 时间格式化
// ----------------------------------------------------------------------------

/// 从 markdown 正文提取 (标题, 分类)：
/// - 标题 = 首个 `# ` 行（trim 后非空）；无则回退文件名去后缀；
/// - 分类 = frontmatter `category: xxx`（首个 `---` 块内）；否则按一级子
///   目录名（`dev/x.md` → `dev`）；根目录文件 → `docs`。
fn extract_title_and_category(markdown: &str, rel_path: &str) -> (String, String) {
    // frontmatter：首行是 `---` 时到下一个 `---` 为元数据块，正文从其后开始。
    let mut frontmatter = String::new();
    let mut body = markdown;
    if markdown.lines().next().is_some_and(|l| l.trim() == "---") {
        let rest = &markdown[markdown.find('\n').map(|i| i + 1).unwrap_or(markdown.len())..];
        if let Some(end) = rest.lines().position(|l| l.trim() == "---") {
            let after = &rest[rest
                .lines()
                .take(end + 1)
                .map(|l| l.len() + 1)
                .sum::<usize>()
                .min(rest.len())..];
            frontmatter = rest.lines().take(end).collect::<Vec<_>>().join("\n");
            body = after;
        }
    }

    // 标题：正文首个 `# ` 行。
    let title = body
        .lines()
        .find(|l| l.starts_with("# ") && !l[2..].trim().is_empty())
        .map(|l| l[2..].trim().to_string())
        .unwrap_or_else(|| {
            rel_path
                .rsplit('/')
                .next()
                .unwrap_or(rel_path)
                .trim_end_matches(".md")
                .to_string()
        });

    // 分类：frontmatter > 一级子目录 > docs。
    let category = frontmatter
        .lines()
        .find_map(|l| {
            let v = l.trim().strip_prefix("category:")?.trim();
            (!v.is_empty()).then(|| v.to_string())
        })
        .unwrap_or_else(|| {
            let normalized = rel_path.replace('\\', "/");
            match normalized.split_once('/') {
                Some((dir, _)) if !dir.is_empty() => dir.to_string(),
                _ => "docs".to_string(),
            }
        });
    (title, category)
}

/// 读文件 + 提取（索引扫描路径用；读取失败回退文件名标题，分类仍按路径推断）。
fn read_title_and_category(rel: &str, abs: &Path) -> (String, String) {
    match std::fs::read_to_string(abs) {
        Ok(content) => extract_title_and_category(&content, rel),
        Err(_) => {
            let fallback_title = rel
                .rsplit('/')
                .next()
                .unwrap_or(rel)
                .trim_end_matches(".md")
                .to_string();
            let normalized = rel.replace('\\', "/");
            let category = normalized
                .split_once('/')
                .filter(|(dir, _)| !dir.is_empty())
                .map(|(dir, _)| dir.to_string())
                .unwrap_or_else(|| "docs".to_string());
            (fallback_title, category)
        }
    }
}

/// 目录 mtime（读失败 → None；缓存失效判定用）。
fn dir_mtime(dir: &Path) -> Option<SystemTime> {
    std::fs::metadata(dir).ok().and_then(|m| m.modified().ok())
}

/// 文档根解析：env → 缺省（106 checkout）→ 二进制旁 ./docs → ../../docs。
/// 全部不存在 → None（降级模式）。返回首个**存在**的目录。
fn resolve_root() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(env_dir) = std::env::var("NEXOS_DEVDOCS_DIR") {
        if !env_dir.trim().is_empty() {
            candidates.push(PathBuf::from(env_dir));
        }
    }
    candidates.push(PathBuf::from(DEFAULT_DEVDOCS_DIR));
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            candidates.push(exe_dir.join("docs"));
            // workspace 内 target/{debug,release} 运行形态：二进制在根下两级。
            candidates.push(exe_dir.join("../..").join("docs"));
        }
    }
    candidates.into_iter().find(|p| p.is_dir())
}

/// 联邦回退源节点解析：env `NEXOS_DEVDOCS_FALLBACK_URL`（trim + 去尾 `/`；
/// 空/未设 → None = 纯降级模式）。
fn fallback_from_env() -> Option<String> {
    std::env::var(FALLBACK_ENV)
        .ok()
        .map(|u| u.trim().trim_end_matches('/').to_string())
        .filter(|u| !u.is_empty())
}

/// SystemTime → ISO 本地时间（失败 → None）。
fn system_time_to_iso(t: SystemTime) -> Option<String> {
    let dt: chrono::DateTime<chrono::Local> = t.into();
    Some(dt.format("%Y-%m-%dT%H:%M:%S%:z").to_string())
}

/// 当前 Unix epoch 秒。
fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ----------------------------------------------------------------------------
// AI 翻译：纯函数（分块 / prompt / 缓存 / 围栏剥离）
// ----------------------------------------------------------------------------

/// 行是否围栏线（``` / ~~~ 开头，可带语言名）。
fn is_fence_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("```") || t.starts_with("~~~")
}

/// 把 markdown 切成 (frontmatter, 翻译块列表)：
/// - frontmatter（首个 `---` 元数据块，含首尾 `---` 行）**不翻译**原样回接——
///   `category:` 等元数据是索引契约，翻译会破坏分类；
/// - 正文按**二级标题**（`## ` 行首）分节（fence 内的 `##` 不算）；
/// - 节 ≤`max_chars` → 单块；超长 → 按空行段落（fence 内空行不算边界）累积
///   切块；单段落仍超长 → 按行硬切（病态单行再按字符切，不 panic）。
fn split_translation_chunks(markdown: &str, max_chars: usize) -> (String, Vec<String>) {
    let (frontmatter, body) = split_frontmatter(markdown);

    // —— 按 ## 分节（fence 感知：围栏内的 `## ` 是代码内容，不是标题）——
    let mut sections: Vec<Vec<&str>> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut in_fence = false;
    for line in body.lines() {
        if is_fence_line(line) {
            in_fence = !in_fence;
        }
        if !in_fence && line.starts_with("## ") && !cur.is_empty() {
            sections.push(std::mem::take(&mut cur));
        }
        cur.push(line);
    }
    if !cur.is_empty() {
        sections.push(cur);
    }

    // —— 节 → 块（≤max_chars；超长段落细分）——
    let mut chunks: Vec<String> = Vec::new();
    for section in sections {
        let section_text = section.join("\n");
        if section_text.chars().count() <= max_chars {
            chunks.push(section_text);
            continue;
        }
        let mut acc = String::new();
        for para in split_paragraphs(&section_text) {
            if para.chars().count() > max_chars {
                if !acc.is_empty() {
                    chunks.push(std::mem::take(&mut acc));
                }
                chunks.extend(hard_split_by_lines(&para, max_chars));
                continue;
            }
            if acc.is_empty() {
                acc = para;
            } else if acc.chars().count() + 2 + para.chars().count() <= max_chars {
                acc.push_str("\n\n");
                acc.push_str(&para);
            } else {
                chunks.push(std::mem::take(&mut acc));
                acc = para;
            }
        }
        if !acc.is_empty() {
            chunks.push(acc);
        }
    }
    (frontmatter, chunks)
}

/// 分离 frontmatter：首行 `---` 且后续存在闭合 `---` 行 →
/// (含首尾 `---` 的完整块 + "\n", 正文)；否则 ("", 原文)。
fn split_frontmatter(markdown: &str) -> (String, &str) {
    let mut lines = markdown.lines();
    if lines.next().map_or(true, |l| l.trim() != "---") {
        return (String::new(), markdown);
    }
    let mut consumed = markdown.find('\n').map_or(markdown.len(), |i| i + 1);
    let mut fm = String::from("---\n");
    for line in lines {
        consumed += line.len() + 1;
        if line.trim() == "---" {
            fm.push_str("---\n");
            let rest = &markdown[consumed.min(markdown.len())..];
            return (fm, rest.trim_start_matches('\n'));
        }
        fm.push_str(line);
        fm.push('\n');
    }
    (String::new(), markdown)
}

/// 按空行切段落（fence 感知：围栏内的空行是代码内容的一部分）。
/// 段落 = 连续非空行（fence 整体归入其所在段落），行内 join("\n")。
fn split_paragraphs(text: &str) -> Vec<String> {
    let mut paras: Vec<String> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        if is_fence_line(line) {
            in_fence = !in_fence;
        }
        if line.trim().is_empty() && !in_fence {
            if !cur.is_empty() {
                paras.push(cur.join("\n"));
                cur = Vec::new();
            }
        } else {
            cur.push(line);
        }
    }
    if !cur.is_empty() {
        paras.push(cur.join("\n"));
    }
    paras
}

/// 按行硬切（每片 ≤max_chars；病态单行超长再按字符切片，char_indices 防越界）。
fn hard_split_by_lines(text: &str, max_chars: usize) -> Vec<String> {
    let mut pieces: Vec<String> = Vec::new();
    let mut acc = String::new();
    for line in text.lines() {
        if line.chars().count() > max_chars {
            if !acc.is_empty() {
                pieces.push(std::mem::take(&mut acc));
            }
            let mut piece = String::new();
            for ch in line.chars() {
                piece.push(ch);
                if piece.chars().count() >= max_chars {
                    pieces.push(std::mem::take(&mut piece));
                }
            }
            continue;
        }
        if acc.is_empty() {
            acc = line.to_string();
        } else if acc.chars().count() + 1 + line.chars().count() <= max_chars {
            acc.push('\n');
            acc.push_str(line);
        } else {
            pieces.push(std::mem::take(&mut acc));
            acc = line.to_string();
        }
    }
    if !acc.is_empty() {
        pieces.push(acc);
    }
    pieces
}

/// 剥掉模型偶尔包裹整个回复的外层代码围栏（```markdown\n…\n```）：
/// 仅当首行是**裸围栏或翻译标记类 info**（```/```markdown/```md/```text/
/// ```plain——正文自身以业务代码块开头时 info 是 rust/json 等语言名，不剥）
/// 且末行为围栏线、剥后内部围栏线计数为偶数时才剥。
fn strip_outer_fence(s: &str) -> String {
    let t = s.trim();
    let lines: Vec<&str> = t.lines().collect();
    if lines.len() < 2 {
        return t.to_string();
    }
    let first = lines[0].trim();
    let last = lines[lines.len() - 1].trim();
    let wrapper_info_ok = |info: &str| {
        matches!(info.trim(), "" | "markdown" | "md" | "text" | "plain")
    };
    let first_ok = ["```", "~~~"]
        .iter()
        .any(|f| first.starts_with(f) && wrapper_info_ok(&first[f.len()..]));
    let last_ok = last == "```" || last == "~~~";
    if !first_ok || !last_ok {
        return t.to_string();
    }
    let inner = lines[1..lines.len() - 1].join("\n");
    // 内部围栏必须配平（toggle 走一遍终态必须出围栏）——不配平说明外层
    // 本身可能就是正文代码块的一部分，宁可不剥。
    let balanced = {
        let mut in_fence = false;
        for l in inner.lines() {
            if is_fence_line(l) {
                in_fence = !in_fence;
            }
        }
        !in_fence
    };
    if !balanced {
        return t.to_string();
    }
    inner.trim().to_string()
}

/// 翻译系统提示词（分块逐条下发；规则即设计定稿的 prompt 契约）。
fn translation_system_prompt(lang: TargetLang) -> String {
    format!(
        "你是专业的技术文档翻译引擎。把用户给出的 Markdown 片段从简体中文翻译成{}，\
输出规则（严格遵守）：\n\
1. 只输出译文本身（保持 Markdown 格式），不要任何解释、前言、后记，\
也不要用代码围栏把整个回复包起来。\n\
2. 代码块、行内代码、命令、URL、文件路径原样保留，不翻译其中的内容。\n\
3. mermaid 图、ASCII 图、表格的结构与对齐保持不变（表格单元格里的说明文字可翻译）。\n\
4. Markdown 结构不变：标题层级、列表缩进、引用、粗体/斜体、链接目标不变\
（链接显示文字可翻译）。\n\
5. 术语表（保留原文，一律不翻译）：{}。\n\
6. 保持段落划分：不合并、不拆分、不增删内容，忠实翻译全文。",
        lang.prompt_target(),
        GLOSSARY.join("、")
    )
}

/// 译文缓存是否命中且新鲜：存在、非空、且原文 mtime 不新于缓存文件 mtime
/// （v1 简化：原文更新 → 过期 → miss 重译，旧译不返回）。
fn translation_cache_fresh(cache_path: &Path, source_mtime: Option<SystemTime>) -> Option<String> {
    let cache_mtime = std::fs::metadata(cache_path)
        .ok()
        .and_then(|m| m.modified().ok())?;
    if source_mtime.is_some_and(|src| src > cache_mtime) {
        return None;
    }
    std::fs::read_to_string(cache_path)
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// 原子写（tmp + rename）：翻译途中崩溃不会留半截译文被缓存命中。
fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = PathBuf::from(format!("{}.tmp{}", path.display(), std::process::id()));
    match std::fs::write(&tmp, content).and_then(|()| std::fs::rename(&tmp, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 最小 query 值 percent-encode（联邦透传 lang 用；RFC3986 未保留集直过）。
fn percent_encode_query(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 请求查询参数提取（files.rs 同款语义：`?a=b&c=d`，值 percent-decode）。
fn query_param(path: &str, key: &str) -> Option<String> {
    let q = path.split('?').nth(1)?;
    for kv in q.split('&') {
        let mut it = kv.splitn(2, '=');
        if it.next()? == key {
            let v = percent_decode(it.next().unwrap_or(""));
            return (!v.trim().is_empty()).then_some(v);
        }
    }
    None
}

/// 202 + 任务视图（首次触发与 running 复用同形）。
fn task_accepted(view: TranslateTaskView) -> ApiResponse {
    ApiResponse {
        status: 202,
        body: serde_json::to_value(&view).unwrap_or_else(|_| serde_json::json!({})),
        headers: serde_json::json!({}),
    }
}

/// 生产翻译配置解析（`new()` 路径）：缓存根 env/缺省（create_dir_all 失败 →
/// None 即翻译不可用，503 文案在请求时给出）；网关 URL / 凭据 / 模型 env。
fn resolve_i18n_config() -> Option<I18nConfig> {
    let cache_dir = std::env::var(I18N_DIR_ENV)
        .ok()
        .map(|d| PathBuf::from(d.trim()))
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from(DEFAULT_I18N_DIR));
    if std::fs::create_dir_all(&cache_dir).is_err() {
        eprintln!(
            "[devdocs] 翻译缓存目录不可创建（{}），AI 翻译停用——文档原文不受影响",
            cache_dir.display()
        );
        return None;
    }
    let gateway_url = std::env::var(GATEWAY_URL_ENV)
        .ok()
        .map(|u| u.trim().trim_end_matches('/').to_string())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| DEFAULT_GATEWAY_URL.to_string());
    let gateway_token = std::env::var(GATEWAY_TOKEN_ENV)
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .or_else(admin_token_from_env);
    let model = std::env::var(TRANSLATE_MODEL_ENV)
        .ok()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| DEFAULT_TRANSLATE_MODEL.to_string());
    Some(I18nConfig {
        cache_dir,
        gateway_url,
        gateway_token,
        model,
    })
}

/// admin token 读取（media_gen.rs 同款语义：NEXOS_ADMIN_TOKEN 优先，回落
/// OS_ADMIN_TOKEN；trim 后非空才算启用）——网关服务端凭据的回落通道。
fn admin_token_from_env() -> Option<String> {
    std::env::var("NEXOS_ADMIN_TOKEN")
        .ok()
        .or_else(|| std::env::var("OS_ADMIN_TOKEN").ok())
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

// ----------------------------------------------------------------------------
// AI 翻译：后台任务（逐块经网关 chat/completions，完成原子写缓存）
// ----------------------------------------------------------------------------

/// 后台翻译任务参数（全部 owned——tokio::spawn 需要 'static）。
struct TranslateJob {
    task_id: String,
    lang: TargetLang,
    /// 缓存键相对路径（safe_rel，日志展示用）。
    path: String,
    /// frontmatter 原样回接（不翻译）。
    frontmatter: String,
    chunks: Vec<String>,
    cache_path: PathBuf,
    gateway_url: String,
    gateway_token: String,
    model: String,
    registry: Arc<TranslateRegistry>,
}

/// 后台翻译主体：逐块调网关（顺序执行——本地模型带宽有限，顺序也带来自然
/// 的进度语义「块 i/N」）；全部成功 → 拼接 + frontmatter 回接 + 原子写缓存。
async fn run_translation(job: TranslateJob) {
    let n = job.chunks.len();
    let mut translated: Vec<String> = Vec::with_capacity(n);
    for (i, chunk) in job.chunks.iter().enumerate() {
        match translate_chunk_via_gateway(&job, chunk).await {
            Ok(t) => {
                let chars = t.chars().count();
                translated.push(t);
                job.registry.chunk_done(&job.task_id);
                job.registry.log(
                    &job.task_id,
                    &format!("✔ 块 {}/{} 完成（{chars} 字符）", i + 1, n),
                );
            }
            Err(e) => {
                eprintln!(
                    "[devdocs] 翻译任务失败：{}（{} → {}）：{e}",
                    job.task_id,
                    job.path,
                    job.lang.dir_name()
                );
                job.registry.finish(
                    &job.task_id,
                    "error",
                    &format!("✘ 块 {}/{} 失败：{e}", i + 1, n),
                    Some(e),
                );
                return;
            }
        }
    }
    // 拼接：frontmatter（原样）+ 各块译文（\n\n 连接）。
    let mut out = String::new();
    if !job.frontmatter.is_empty() {
        out.push_str(job.frontmatter.trim_end());
        out.push_str("\n\n");
    }
    out.push_str(&translated.join("\n\n"));
    out.push('\n');
    match atomic_write(&job.cache_path, &out) {
        Ok(()) => {
            eprintln!(
                "[devdocs] 翻译任务完成：{}（{} → {}，{n} 块 → {}）",
                job.task_id,
                job.path,
                job.lang.dir_name(),
                job.cache_path.display()
            );
            job.registry
                .finish(&job.task_id, "done", "✔ 翻译完成并写入缓存", None);
        }
        Err(e) => {
            eprintln!(
                "[devdocs] 翻译缓存写入失败：{}（{}）：{e}",
                job.task_id,
                job.cache_path.display()
            );
            job.registry.finish(
                &job.task_id,
                "error",
                &format!("✘ 译文缓存写入失败：{e}"),
                Some(format!("译文缓存写入失败（{}）：{e}", job.cache_path.display())),
            );
        }
    }
}

/// 单块翻译的错误分类（决定是否走禁思考重试）。
enum ChunkError {
    /// 输出预算被**思考段**占用：content 空 + reasoning 出现，或 finish=length
    /// （106 的 vLLM qwen3.5-9b 不回传 reasoning_content，思考 token 藏在
    /// 预算里烧完——finish=length 是该形态的判据）。
    ThinkingOccupied(String),
    /// 其他网关/解析错误（文案即终态，不重试）。
    Other(String),
}

impl ChunkError {
    /// 终态文案（含重试已做尽的说明）。
    fn into_final(self) -> String {
        match self {
            ChunkError::Other(msg) => msg,
            // 走到这里说明 chat_template_kwargs 与 /no_think 两次尝试都被
            // 思考占用——文案明确区分（不与普通解析错误混淆）。
            ChunkError::ThinkingOccupied(detail) => format!(
                "模型思考段占用输出预算（已用 chat_template_kwargs 禁思考并 \
                 /no_think 重试一次仍空）：{detail}"
            ),
        }
    }
}

/// 单块翻译（外层）：先按官方开关禁思考直译；输出被思考段占用 → 追加
/// Qwen3 软开关 `/no_think` 重试一次；仍空才落 error（真机验证结论见
/// DEVDOCS_DEV_CENTER.md §5.2：106 上主开关生效，软开关在该后端无效——
/// 保留软开关是为换非 vLLM 后端时的降级通道）。
async fn translate_chunk_via_gateway(job: &TranslateJob, chunk: &str) -> Result<String, String> {
    match translate_attempt(job, chunk, false).await {
        Ok(t) => Ok(t),
        Err(ChunkError::ThinkingOccupied(detail)) => {
            job.registry.log(
                &job.task_id,
                &format!("⚠ 输出被思考段占用（{detail}）——/no_think 软开关重试一次"),
            );
            eprintln!(
                "[devdocs] 翻译块输出被思考段占用，/no_think 重试：{}（{}）",
                job.task_id, job.path
            );
            translate_attempt(job, chunk, true)
                .await
                .map_err(ChunkError::into_final)
        }
        Err(e) => Err(e.into_final()),
    }
}

/// 单次翻译尝试：POST `{gateway}/api/v1/gateway/v1/chat/completions`（Bearer
/// 服务端凭据；stream:false；[`CHUNK_TIMEOUT`] 超时）。`soft_no_think` = 在
/// user 内容尾追加 Qwen3 软开关（仅思考占用后的重试路径使用）。
async fn translate_attempt(
    job: &TranslateJob,
    chunk: &str,
    soft_no_think: bool,
) -> Result<String, ChunkError> {
    let url = format!("{}/api/v1/gateway/v1/chat/completions", job.gateway_url);
    let system = translation_system_prompt(job.lang);
    let user = if soft_no_think {
        format!("{chunk}\n\n/no_think")
    } else {
        chunk.to_string()
    };
    // 输出预算（动态）：输入字符数/2 + 2048——中文→英/繁 token 量约为字符
    // 一半，加 2048 裕量；静态小值会被思考段或长译文吃穿（finish=length）。
    let max_tokens: usize = (system.chars().count() + user.chars().count()) / 2 + 2048;
    let body = serde_json::json!({
        "model": job.model,
        "messages": [
            {"role": "system", "content": system},
            {"role": "user", "content": user},
        ],
        "stream": false,
        "temperature": 0.2,
        "max_tokens": max_tokens,
        // vLLM Qwen3 系官方思考开关（网关 body 原样透传）——106 真机实测
        // 生效：content 直出、completion_tokens 个位数、无 reasoning。
        "chat_template_kwargs": {"enable_thinking": false},
    });
    let resp = TRANSLATE_HTTP
        .post(&url)
        .bearer_auth(&job.gateway_token)
        .timeout(CHUNK_TIMEOUT)
        .json(&body)
        .send()
        .await
        .map_err(|e| ChunkError::Other(format!("网关请求失败（{url}）：{e}")))?;
    let status = resp.status().as_u16();
    let text = resp
        .text()
        .await
        .map_err(|e| ChunkError::Other(format!("网关响应读取失败：{e}")))?;
    if status != 200 {
        let detail = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| {
                v["error"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| v["message"].as_str().map(str::to_string))
            })
            .unwrap_or_else(|| tail_chars(&text, 200));
        return Err(ChunkError::Other(match status {
            404 | 502 | 503 => format!("{}（网关 {status}：{detail}）", no_model_msg(job.lang)),
            401 => format!(
                "翻译调用的网关凭据无效（{detail}）：配置 NEXOS_DEVDOCS_GATEWAY_TOKEN 为有效 \
                 sk-os- 网关令牌（或把 NEXOS_ADMIN_TOKEN 的值注册为网关令牌）后重启"
            ),
            429 => format!("网关令牌配额已用尽（{detail}），无法生成翻译"),
            _ => format!("网关返回 {status}：{detail}"),
        }));
    }
    let v: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| ChunkError::Other(format!("网关响应非 JSON：{e}")))?;
    let choice = &v["choices"][0];
    let content = choice["message"]["content"].as_str();
    let reasoning = choice["message"]["reasoning_content"]
        .as_str()
        .or_else(|| choice["message"]["reasoning"].as_str());
    let finish = choice["finish_reason"].as_str().unwrap_or("");
    let completion_tokens = v["usage"]["completion_tokens"].as_i64().unwrap_or(-1);
    match content.filter(|c| !c.trim().is_empty()) {
        Some(c) => {
            let stripped = strip_outer_fence(c);
            if stripped.trim().is_empty() {
                return Err(ChunkError::Other(
                    "网关返回了空译文（拒绝写入空缓存）".into(),
                ));
            }
            Ok(stripped)
        }
        None => {
            // content 空：区分思考占用（reasoning 出现，或预算被烧穿 finish=length
            // ——106 qwen3.5-9b 思考 token 不以 reasoning_content 回传）。
            let thinking = reasoning.is_some_and(|r| !r.trim().is_empty()) || finish == "length";
            if thinking {
                Err(ChunkError::ThinkingOccupied(format!(
                    "content 为空，reasoning {}，finish={finish}，completion_tokens={completion_tokens} / max_tokens={max_tokens}",
                    if reasoning.is_some() { "非空" } else { "未回传" }
                )))
            } else {
                Err(ChunkError::Other(
                    "网关响应缺少 choices[0].message.content（且无思考段迹象）".into(),
                ))
            }
        }
    }
}

/// 字符串截尾（保留最后 n 个字符；agenthub_toolchain 同款）。
fn tail_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().skip(s.chars().count() - n).collect()
}

// ----------------------------------------------------------------------------
// 构造响应的小工具（update.rs 同款）
// ----------------------------------------------------------------------------

/// 构造一条 RouteSpec（component 固定 `devdocs`）。
fn spec(method: HttpMethod, path: &str) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: COMPONENT.to_string(),
        requires_auth: false,
        required_roles: vec![],
    }
}

/// 构造一个 200 JSON 响应（空 headers）。
fn ok_json(body: serde_json::Value) -> ApiResponse {
    ApiResponse {
        status: 200,
        body,
        headers: serde_json::json!({}),
    }
}

/// 构造一个最小 JSON 错误响应。
fn error_response(status: u16, msg: &str) -> ApiResponse {
    ApiResponse {
        status,
        body: serde_json::json!({"error": msg}),
        headers: serde_json::json!({}),
    }
}

/// 把可序列化结果转成 Value，序列化失败统一映射为 Internal。
fn to_value<T: Serialize>(v: &T) -> Result<serde_json::Value, ApiGatewayError> {
    serde_json::to_value(v).map_err(|e| ApiGatewayError::Internal(format!("响应序列化失败: {e}")))
}

/// 从请求路径剥离 `?query` 后的纯 path。
fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

// ----------------------------------------------------------------------------
// RouteHandler 实现
// ----------------------------------------------------------------------------

#[async_trait]
impl RouteHandler for DevDocsRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec(HttpMethod::Get, "/api/v1/devdocs/index"),
            spec(HttpMethod::Get, "/api/v1/devdocs/doc/*"),
            spec(HttpMethod::Get, "/api/v1/devdocs/translate/tasks/:id"),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // —— GET /api/v1/devdocs/index —— 文档索引（缓存 30s；
            //    本地无根且配置联邦源 → 代理透传，见 federated_index）
            (HttpMethod::Get, ["api", "v1", "devdocs", "index"]) => {
                if let Some(resp) = self.federated_index().await {
                    return Ok(resp);
                }
                Ok(ok_json(to_value(&self.index())?))
            }

            // —— GET /api/v1/devdocs/doc/*path —— 单篇原文（路径安全三闸；
            //    本地无根且配置联邦源 → 本地仅拒 `..`，其余代理透传）。
            //    ?lang=en|zh-TW → AI 翻译管线（缓存/任务/降级，见模块文档）。
            (HttpMethod::Get, ["api", "v1", "devdocs", "doc", rest @ ..]) => {
                if rest.is_empty() {
                    return Ok(error_response(
                        400,
                        "缺少文档路径（/api/v1/devdocs/doc/<path>）",
                    ));
                }
                let rel = rest.join("/");
                // URL 解码（%2e%2e 等编码穿越经解码后再走拦截）。
                let rel = percent_decode(&rel);
                // 语言参数：缺省/zh 原文直读（零开销）；非法值 400。
                let lang_raw = query_param(&req.path, "lang");
                let lang = match TargetLang::parse(lang_raw.as_deref()) {
                    Ok(l) => l,
                    Err(msg) => return Ok(error_response(400, &msg)),
                };
                let retry = query_param(&req.path, "retry").is_some_and(|v| v == "1");
                if self.root.is_none() && self.fallback.is_some() {
                    // 降级 + 联邦：本地先拒 `..`（双保险，主闸在源节点），
                    // lang 查询一并透传源节点（译文由源节点缓存/翻译）。
                    if rel.split(['/', '\\']).any(|seg| seg == "..") {
                        return Ok(error_response(403, "路径越界（仅允许文档根内）"));
                    }
                    return Ok(self.federated_doc(&rel, lang_raw.as_deref()).await);
                }
                match lang {
                    None => match self.read_doc(&rel) {
                        Ok((doc, _)) => Ok(ok_json(to_value(&doc)?)),
                        Err((status, msg)) => Ok(error_response(status, &msg)),
                    },
                    Some(lang) => Ok(self.doc_with_lang(&rel, lang, retry).await),
                }
            }

            // —— GET /api/v1/devdocs/translate/tasks/:id —— 翻译任务视图
            //    （状态机 + 环形日志；running 超时惰性转 error）
            (HttpMethod::Get, ["api", "v1", "devdocs", "translate", "tasks", id]) => {
                self.translate.reap_stale(id);
                match self.translate.snapshot(id) {
                    Some(view) => Ok(ok_json(to_value(&view)?)),
                    None => Ok(error_response(404, "翻译任务不存在（可能来自联邦源节点，或已被回收）")),
                }
            }

            _ => Ok(error_response(404, "未知 devdocs 端点")),
        }
    }
}

/// 最小 percent-decode（仅路径段用；`+` 不视为空格——路径语义）。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let hex = |b: u8| (b as char).to_digit(16);
    let mut i = 0;
    while i < bytes.len() {
        // `%HH` 三连且两位均为 hex → 解码为一个字节；否则原样保留。
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(((h << 4) | l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ----------------------------------------------------------------------------
// 单元测试（≥6：扫描/分类/标题/缓存/读取/穿越拒绝/非 md 拒绝/降级/联邦回退/
// 路由约定）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;

    /// 临时目录 guard（drop 清理；workspace 未注册 tempfile，update.rs 同款自管）。
    struct TempDirGuard(PathBuf);
    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_root(tag: &str) -> (PathBuf, TempDirGuard) {
        let dir =
            std::env::temp_dir().join(format!("nexos-devdocs-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dev")).unwrap();
        let guard = TempDirGuard(dir.clone());
        (dir, guard)
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn get_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    /// 等目录 mtime 可见变化（部分文件系统 mtime 粒度 1s——直接 touch 未来时间）。
    fn bump_dir_mtime(dir: &Path) {
        let future = SystemTime::now() + Duration::from_secs(3600);
        let _ = std::fs::File::open(dir).and_then(|f| f.set_modified(future));
    }

    // ============ 1. index 扫描：根 + 一级子目录、分类、标题提取 ============

    #[tokio::test]
    async fn index_scans_root_and_subdir_with_category_and_title() {
        let (root, _g) = temp_root("scan");
        write(&root.join("README.md"), "# 总览\n\n系统总览文档。\n");
        write(
            &root.join("dev/01-app.md"),
            "---\ncategory: 开发者指南\n---\n\n# 应用开发\n\n全流程。\n",
        );
        write(
            &root.join("dev/02-install.md"),
            "# 安装自己的应用\n\n正文。\n",
        );
        write(&root.join("dev/notes.txt"), "非 md 不入索引");

        let h = DevDocsRouteHandler::with_root(Some(root.clone()));
        let resp = h.handle(get_req("/api/v1/devdocs/index")).await.unwrap();
        assert_eq!(resp.status, 200, "body: {resp:?}");
        assert_eq!(resp.body["source_available"], true);
        let docs = resp.body["docs"].as_array().unwrap();
        assert_eq!(docs.len(), 3, "根 1 篇 + dev/ 2 篇，txt 不入索引");

        // 分类：frontmatter 优先（01-app.md）、子目录名（02-install.md）、根=docs。
        let by_path = |p: &str| {
            docs.iter()
                .find(|d| d["path"] == p)
                .unwrap_or_else(|| panic!("缺 {p}"))
                .clone()
        };
        assert_eq!(by_path("README.md")["category"], "docs");
        assert_eq!(by_path("README.md")["title"], "总览");
        assert_eq!(
            by_path("dev/01-app.md")["category"],
            "开发者指南",
            "frontmatter category 优先"
        );
        assert_eq!(
            by_path("dev/01-app.md")["title"],
            "应用开发",
            "frontmatter 后的正文标题"
        );
        assert_eq!(by_path("dev/02-install.md")["category"], "dev");
        assert_eq!(by_path("dev/02-install.md")["title"], "安装自己的应用");
        // size / mtime 存在且非负
        assert!(by_path("README.md")["size"].as_u64().unwrap() > 0);
        assert!(by_path("README.md")["mtime"].is_string());
        // categories 含三个分组
        let cats = resp.body["categories"].as_array().unwrap();
        assert!(cats.iter().any(|c| c == "docs"));
        assert!(cats.iter().any(|c| c == "开发者指南"));
        assert!(cats.iter().any(|c| c == "dev"));
    }

    // ============ 2. 无 `# ` 标题回退文件名 ============

    #[tokio::test]
    async fn index_title_falls_back_to_filename() {
        let (root, _g) = temp_root("title");
        write(
            &root.join("adr/ADR-X.md"),
            "没有一级标题的文档。\n\n## 只有二级\n",
        );

        let h = DevDocsRouteHandler::with_root(Some(root));
        let resp = h.handle(get_req("/api/v1/devdocs/index")).await.unwrap();
        let docs = resp.body["docs"].as_array().unwrap();
        assert_eq!(docs[0]["title"], "ADR-X", "无 # 标题回退文件名去后缀");
        assert_eq!(docs[0]["category"], "adr");
    }

    // ============ 3. 缓存：30s 内复用，目录 mtime 变化立即失效 ============

    #[tokio::test]
    async fn index_cache_reused_until_dir_mtime_changes() {
        let (root, _g) = temp_root("cache");
        write(&root.join("a.md"), "# A\n");

        let h = DevDocsRouteHandler::with_root(Some(root.clone()));
        let r1 = h.handle(get_req("/api/v1/devdocs/index")).await.unwrap();
        assert_eq!(r1.body["docs"].as_array().unwrap().len(), 1);
        assert_eq!(h.scan_count.load(Ordering::SeqCst), 1, "首次真实扫描");

        // 第二次：TTL 内且 mtime 未变 → 命中缓存，不重扫。
        let r2 = h.handle(get_req("/api/v1/devdocs/index")).await.unwrap();
        assert_eq!(r2.body["docs"].as_array().unwrap().len(), 1);
        assert_eq!(h.scan_count.load(Ordering::SeqCst), 1, "缓存命中不重扫");

        // 新增文档（目录 mtime 变化）→ 立即失效重扫，新文档可见。
        write(&root.join("b.md"), "# B\n");
        bump_dir_mtime(&root);
        let r3 = h.handle(get_req("/api/v1/devdocs/index")).await.unwrap();
        assert_eq!(
            r3.body["docs"].as_array().unwrap().len(),
            2,
            "新增文档即时可见"
        );
        assert_eq!(
            h.scan_count.load(Ordering::SeqCst),
            2,
            "mtime 变化后恰好再扫一次"
        );
    }

    // ============ 4. doc 读取：正常路径（markdown 原文 + 标题 + mtime）============

    #[tokio::test]
    async fn doc_read_returns_markdown_source() {
        let (root, _g) = temp_root("read");
        let md = "# 标题\n\n```rust\nfn main() {}\n```\n";
        write(&root.join("dev/guide.md"), md);

        let h = DevDocsRouteHandler::with_root(Some(root));
        let resp = h
            .handle(get_req("/api/v1/devdocs/doc/dev/guide.md"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["path"], "dev/guide.md");
        assert_eq!(resp.body["title"], "标题");
        assert_eq!(resp.body["markdown"], md, "markdown 原文逐字节返回");
        assert!(resp.body["mtime"].is_string());
    }

    // ============ 5. doc 穿越/非 md/不存在 全拒绝 ============

    #[tokio::test]
    async fn doc_rejects_traversal_non_md_and_missing() {
        let (root, _g) = temp_root("reject");
        // 根外放一个可读目标（穿越测试的靶子）。
        let outside = root.parent().unwrap().join("secret-target.md");
        write(&outside, "# 秘密\n");
        write(&root.join("ok.md"), "# OK\n");

        let h = DevDocsRouteHandler::with_root(Some(root.clone()));
        // `..` 穿越（直接编码）。
        let r1 = h
            .handle(get_req("/api/v1/devdocs/doc/../secret-target.md"))
            .await
            .unwrap();
        assert_eq!(r1.status, 403, "body: {r1:?}");
        // 多段穿越 + 非 md。
        let r2 = h
            .handle(get_req("/api/v1/devdocs/doc/dev/../../../../etc/passwd"))
            .await
            .unwrap();
        assert_eq!(r2.status, 400, "passwd 非 .md 先被闸 1 拒");
        // 非 md（仓库常见非文档文件名）。
        let r3 = h
            .handle(get_req("/api/v1/devdocs/doc/Cargo.toml"))
            .await
            .unwrap();
        assert_eq!(r3.status, 400);
        // 百分号编码穿越（%2e%2e = ..）——解码后再走三闸。
        let r4 = h
            .handle(get_req("/api/v1/devdocs/doc/%2e%2e/secret-target.md"))
            .await
            .unwrap();
        assert_eq!(r4.status, 403, "编码穿越解码后拦截");
        // 存在的 .md 正常（对照组）。
        let r5 = h
            .handle(get_req("/api/v1/devdocs/doc/ok.md"))
            .await
            .unwrap();
        assert_eq!(r5.status, 200);
        // 不存在 → 404；空路径 → 400。
        let r6 = h
            .handle(get_req("/api/v1/devdocs/doc/nope.md"))
            .await
            .unwrap();
        assert_eq!(r6.status, 404);
        let r7 = h.handle(get_req("/api/v1/devdocs/doc/")).await.unwrap();
        assert_eq!(r7.status, 400);
        let _ = std::fs::remove_file(&outside);
    }

    // ============ 6. 目录不存在降级：空清单 + 提示，不 crash ============

    #[tokio::test]
    async fn degraded_mode_when_root_missing() {
        let h = DevDocsRouteHandler::with_root(Some(PathBuf::from("/nonexistent/devdocs")));
        assert!(h.root_path().is_none(), "不存在的目录应过滤为降级模式");
        let resp = h.handle(get_req("/api/v1/devdocs/index")).await.unwrap();
        assert_eq!(resp.status, 200, "降级不报 500");
        assert_eq!(resp.body["source_available"], false);
        assert_eq!(resp.body["docs"].as_array().unwrap().len(), 0);
        assert!(
            resp.body["note"].as_str().unwrap().contains("本仓库节点"),
            "提示文案指回主节点"
        );
        let doc = h
            .handle(get_req("/api/v1/devdocs/doc/README.md"))
            .await
            .unwrap();
        assert_eq!(doc.status, 503, "降级模式下读文档 503+说明");
    }

    // ============ 7. 路由声明与鉴权约定（公开读）============

    #[tokio::test]
    async fn routes_declared_and_public_read() {
        let h = DevDocsRouteHandler::with_root(None);
        let routes = h.routes().await;
        assert_eq!(routes.len(), 3);
        assert!(routes.iter().any(|r| r.path == "/api/v1/devdocs/index"));
        assert!(routes.iter().any(|r| r.path == "/api/v1/devdocs/doc/*"));
        assert!(routes
            .iter()
            .any(|r| r.path == "/api/v1/devdocs/translate/tasks/:id"));
        for r in &routes {
            assert!(!r.requires_auth, "开发期公开读");
            assert!(r.required_roles.is_empty());
            assert_eq!(r.handler_component, "devdocs");
        }
    }

    // ============ 8. percent_decode 边界 ============

    #[test]
    fn percent_decode_boundaries() {
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("%2e%2e"), "..");
        assert_eq!(percent_decode("%2E%2E/x"), "../x", "大写 hex");
        assert_eq!(percent_decode("plain.md"), "plain.md");
        assert_eq!(percent_decode("%zz"), "%zz", "非法序列原样保留");
        assert_eq!(percent_decode("a%2"), "a%2", "截断序列原样保留");
    }

    // ---- 联邦回退（mock：本地起 TcpListener 假源，agent_coord
    //      spawn_mock_webhook 同款手法）----

    /// 起一个本地 mock 联邦源（std::net::TcpListener + 线程）：对任意 GET
    /// 回 200 + 固定 JSON；至多服务 `max` 个连接（`max=1` + 连续两次请求
    /// 即可断言缓存命中不重复回源）。返回源节点 base url。
    fn spawn_mock_fallback(body: &'static str, max: usize) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
        let addr = listener.local_addr().expect("local_addr 失败");
        std::thread::spawn(move || {
            for _ in 0..max {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                // GET 无 body：收一轮请求即回（不必按 Content-Length 等全）。
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}")
    }

    /// 取一个「当前无监听」的本地端口（bind 后立即 drop → 连接必被拒，
    /// 模拟源节点不可达）。
    fn free_closed_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
        let port = listener.local_addr().expect("local_addr 失败").port();
        drop(listener);
        port
    }

    fn degraded_handler_with_fallback(fallback: String) -> DevDocsRouteHandler {
        DevDocsRouteHandler::with_root_and_fallback(
            Some(PathBuf::from("/nonexistent/devdocs")),
            Some(fallback),
        )
    }

    // ============ 9. 联邦回退 index：走代理透传 + 30s 缓存 ============

    #[tokio::test]
    async fn federated_index_proxies_source_and_caches() {
        let payload = r#"{"docs":[{"path":"README.md","title":"来自106","category":"docs","size":12,"mtime":null}],"categories":["docs"],"source_available":true,"root":"/home/oem/NexOS/docs","note":"本地文档服务"}"#;
        // max=1：第二次请求若再回源必然失败落降级——能同时验证透传与缓存。
        let base = spawn_mock_fallback(payload, 1);
        let h = degraded_handler_with_fallback(base.clone());
        assert!(h.root_path().is_none(), "降级模式（本地无根）");
        for i in 0..2 {
            let resp = h.handle(get_req("/api/v1/devdocs/index")).await.unwrap();
            assert_eq!(resp.status, 200, "第 {} 次", i + 1);
            assert_eq!(resp.body["source_available"], true, "透传源节点状态");
            assert_eq!(
                resp.body["note"],
                serde_json::Value::String(format!("联邦文档分发：{base}")),
                "note 覆写为联邦来源提示"
            );
            assert_eq!(
                resp.body["docs"][0]["title"], "来自106",
                "透传源节点索引内容"
            );
        }
    }

    // ============ 10. 联邦回退 index：源不可达 → 现有降级 ============

    #[tokio::test]
    async fn federated_index_unreachable_falls_back_to_degraded() {
        let base = format!("http://127.0.0.1:{}", free_closed_port());
        let h = degraded_handler_with_fallback(base);
        let resp = h.handle(get_req("/api/v1/devdocs/index")).await.unwrap();
        assert_eq!(resp.status, 200, "源不可达仍 200 落降级，不 500");
        assert_eq!(resp.body["source_available"], false);
        assert_eq!(resp.body["docs"].as_array().unwrap().len(), 0);
        assert!(
            resp.body["note"].as_str().unwrap().contains("本仓库节点"),
            "落回现有降级提示"
        );
    }

    // ============ 11. 联邦回退 doc：透传形状（不缓存）============

    #[tokio::test]
    async fn federated_doc_proxies_passthrough_shape() {
        let payload = r##"{"path":"dev/x.md","title":"联邦标题","markdown":"# 联邦标题\n\n正文。\n","mtime":null}"##;
        let base = spawn_mock_fallback(payload, 1);
        let h = degraded_handler_with_fallback(base);
        let resp = h
            .handle(get_req("/api/v1/devdocs/doc/dev/x.md"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["path"], "dev/x.md");
        assert_eq!(resp.body["title"], "联邦标题");
        assert_eq!(
            resp.body["markdown"], "# 联邦标题\n\n正文。\n",
            "markdown 原文逐字节透传"
        );
        assert!(resp.body["mtime"].is_null());
    }

    // ============ 12. 联邦回退 doc：`..` 本地先拒 / 源不可达 503 ============

    #[tokio::test]
    async fn federated_doc_rejects_traversal_and_503_when_source_down() {
        // max=0：`..` 若被误发往源会连接失败——能断言是本地拦截生效。
        let base = spawn_mock_fallback("{}", 0);
        let h = degraded_handler_with_fallback(base);
        let r1 = h
            .handle(get_req("/api/v1/devdocs/doc/../secret.md"))
            .await
            .unwrap();
        assert_eq!(r1.status, 403, "明文穿越本地先拒（不代理）");
        let r2 = h
            .handle(get_req("/api/v1/devdocs/doc/%2e%2e/secret.md"))
            .await
            .unwrap();
        assert_eq!(r2.status, 403, "编码穿越解码后本地同样先拒");
        // 源不可达 → 503 + 说明（不是 404 / 500）。
        let down =
            degraded_handler_with_fallback(format!("http://127.0.0.1:{}", free_closed_port()));
        let r3 = down
            .handle(get_req("/api/v1/devdocs/doc/README.md"))
            .await
            .unwrap();
        assert_eq!(r3.status, 503);
        assert!(
            r3.body["error"]
                .as_str()
                .unwrap()
                .contains("联邦文档源不可达"),
            "body: {r3:?}"
        );
    }

    // ============ 22. 联邦 doc：?lang= 透传源节点（译文由源节点负责）============

    #[tokio::test]
    async fn federated_doc_forwards_lang_query_to_source() {
        // mock 源（真 TCP，记录请求行）：对 GET 回 200 + DocResp 形状 JSON。
        fn respond_doc(_req: &RecordedGwReq, _call: usize) -> (u16, String) {
            let body = serde_json::json!({
                "path": "a.md", "title": "联邦译文", "markdown": "# 联邦译文\n", "mtime": null,
            })
            .to_string();
            (200, body)
        }
        let recorded: Arc<Mutex<Vec<RecordedGwReq>>> = Arc::new(Mutex::new(Vec::new()));
        let base = spawn_mock_gateway(respond_doc, Arc::clone(&recorded));
        let h = degraded_handler_with_fallback(base);
        let resp = h
            .handle(get_req("/api/v1/devdocs/doc/a.md?lang=zh-TW"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "body: {resp:?}");
        assert_eq!(resp.body["title"], "联邦译文", "透传源节点译文");
        let reqs = recorded.lock().unwrap().clone();
        assert_eq!(reqs.len(), 1);
        assert!(
            reqs[0].path.contains("/api/v1/devdocs/doc/a.md?lang=zh-TW"),
            "lang 查询透传源节点：{}",
            reqs[0].path
        );
    }

    // =====================================================================
    // AI 翻译管线（mock 网关 = 真 TCP；不跑真实翻译——质量由 106 实测验收）
    // =====================================================================

    /// mock 网关记录的一条请求（断言凭据/模型/分块内容用）。
    #[derive(Debug, Clone)]
    struct RecordedGwReq {
        path: String,
        auth: String,
        body: serde_json::Value,
    }

    /// 读一条完整 HTTP 请求（headers + Content-Length body）。
    fn read_http_request(stream: &mut std::net::TcpStream) -> Option<(String, Vec<u8>)> {
        use std::io::Read;
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 8192];
        // 1) 读到 header 末尾（\r\n\r\n）。
        let header_end = loop {
            if let Some(p) = buf
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
            {
                break p + 4;
            }
            let n = stream.read(&mut tmp).ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&tmp[..n]);
        };
        let headers = String::from_utf8_lossy(&buf[..header_end]).into_owned();
        // 2) Content-Length 继续读 body。
        let cl: usize = headers
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.trim()
                    .eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse().ok())?
            })
            .unwrap_or(0);
        while buf.len() < header_end + cl {
            let n = stream.read(&mut tmp).ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        Some((headers, buf[header_end..].to_vec()))
    }

    /// 起 mock API 网关（真 TCP，std 线程逐连接处理）：`respond(req, call)`
    /// 对每个请求计算 (status, JSON body)，`call` = 本连接前的已记录请求数
    /// （按调用次序变响应的测试用）；请求记录进共享 Vec 供断言。返回
    /// (base_url, 记录句柄)。`respond` 是 fn 指针（服务线程执行，可 sleep）。
    fn spawn_mock_gateway(
        respond: fn(&RecordedGwReq, usize) -> (u16, String),
        recorded: Arc<Mutex<Vec<RecordedGwReq>>>,
    ) -> String {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 失败");
        let addr = listener.local_addr().expect("local_addr 失败");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    break;
                };
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let Some((headers, body)) = read_http_request(&mut stream) else {
                    continue;
                };
                let path = headers.lines().next().unwrap_or_default().to_string();
                let auth = headers
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.trim()
                            .eq_ignore_ascii_case("authorization")
                            .then(|| v.trim().to_string())
                    })
                    .unwrap_or_default();
                let body_json = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                let req = RecordedGwReq {
                    path,
                    auth,
                    body: body_json,
                };
                let call = recorded
                    .lock()
                    .map(|rec| rec.len())
                    .unwrap_or(0);
                let (status, resp_body) = respond(&req, call);
                if let Ok(mut rec) = recorded.lock() {
                    rec.push(req);
                }
                let reason = match status {
                    200 => "OK",
                    401 => "Unauthorized",
                    404 => "Not Found",
                    429 => "Too Many Requests",
                    502 => "Bad Gateway",
                    _ => "Error",
                };
                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{resp_body}",
                    resp_body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.flush();
            }
        });
        format!("http://{addr}")
    }

    /// 翻译链路测试构造：临时文档根 + 临时缓存根 + 指向 mock 网关的配置。
    fn translate_handler(
        root: &Path,
        cache_dir: &Path,
        gateway_url: String,
        token: Option<&str>,
        model: &str,
    ) -> DevDocsRouteHandler {
        DevDocsRouteHandler::with_root_and_i18n(
            Some(root.to_path_buf()),
            I18nConfig {
                cache_dir: cache_dir.to_path_buf(),
                gateway_url,
                gateway_token: token.map(str::to_string),
                model: model.to_string(),
            },
        )
    }

    /// 轮询任务端点直到终态（10s 兜底；返回 tasks/:id 响应 body）。
    async fn wait_task_settled(h: &DevDocsRouteHandler, task_id: &str) -> serde_json::Value {
        for _ in 0..400 {
            let resp = h
                .handle(get_req(&format!(
                    "/api/v1/devdocs/translate/tasks/{task_id}"
                )))
                .await
                .unwrap();
            assert_eq!(resp.status, 200, "任务端点恒 200（存在时）: {resp:?}");
            if resp.body["status"] != "running" {
                return resp.body;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("翻译任务 {task_id} 在 10s 内未到终态");
    }

    /// 「翻译一切」的 mock 网关应答：回显 user 内容加标记（译文可断言顺序）。
    fn gw_echo_translation(req: &RecordedGwReq, _call: usize) -> (u16, String) {
        let user = req.body["messages"][1]["content"].as_str().unwrap_or("");
        let body = serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": format!("[T]{user}[/T]")}}],
        })
        .to_string();
        (200, body)
    }

    /// 无渠道节点的 mock 网关应答（网关真实语义：404 无可用渠道支持该模型）。
    fn gw_no_channel(_req: &RecordedGwReq, _call: usize) -> (u16, String) {
        (404, r#"{"error":"无可用渠道支持该模型"}"#.to_string())
    }

    /// 慢网关（服务线程 sleep 400ms——让任务停留在 running 供并发断言）。
    fn gw_slow_echo(req: &RecordedGwReq, call: usize) -> (u16, String) {
        std::thread::sleep(Duration::from_millis(400));
        gw_echo_translation(req, call)
    }

    /// 思考占用形态（106 真机实测：content=null、reasoning_content 不回传、
    /// finish=length——预算被思考 token 烧穿）；call>0 回正常回显。
    fn gw_thinking_first(req: &RecordedGwReq, call: usize) -> (u16, String) {
        if call == 0 {
            let body = serde_json::json!({
                "choices": [{
                    "message": {"role": "assistant", "content": null, "reasoning_content": null},
                    "finish_reason": "length",
                }],
                "usage": {"completion_tokens": 4096},
            })
            .to_string();
            return (200, body);
        }
        gw_echo_translation(req, call)
    }

    /// 永远思考占用（重试也救不回——终态文案用例）。
    fn gw_always_thinking(_req: &RecordedGwReq, _call: usize) -> (u16, String) {
        let body = serde_json::json!({
            "choices": [{
                "message": {"role": "assistant", "content": null,
                            "reasoning_content": "<think>思考段内容</think>"},
                "finish_reason": "length",
            }],
            "usage": {"completion_tokens": 4096},
        })
        .to_string();
        (200, body)
    }

    /// 三节文档（frontmatter + 引言 + 三个二级标题节，其中一节超长触发段落切分）。
    fn long_doc() -> String {
        let mut overlong = String::from("## 超长节\n\n");
        for i in 0..300 {
            overlong.push_str(&format!("段落 {i}：这是一段用于撑满分块上限的中文说明文字。\n\n"));
        }
        format!(
            "---\ncategory: 测试\n---\n\n# 测试文档\n\n引言段。\n\n## 第一节\n\n内容 A，含 ZFS 与 NodeID 术语。\n\n```rust\n// 代码块内的 ## 不是标题\nfn main() {{}}\n```\n\n{overlong}\n## 末节\n\n收尾。\n"
        )
    }

    // ============ 13. 分块器：二级标题分节 / 上限 / fence 感知 / frontmatter 不译 ============

    #[test]
    fn translate_chunker_sections_cap_fence_and_frontmatter() {
        let (frontmatter, chunks) = split_translation_chunks(&long_doc(), 6000);
        assert_eq!(frontmatter, "---\ncategory: 测试\n---\n", "frontmatter 原样剥离");
        // 引言 1 块；第一节+代码块 1 块；超长节按段落切出多块；末节 1 块。
        assert!(chunks.len() >= 5, "超长节应拆出多块：{}", chunks.len());
        for (i, c) in chunks.iter().enumerate() {
            assert!(
                c.chars().count() <= 6000,
                "块 {i} 超上限：{}",
                c.chars().count()
            );
        }
        // 引言（frontmatter 后正文首块）。
        assert!(
            chunks[0].starts_with("# 测试文档") && chunks[0].contains("引言段"),
            "首块 = 引言"
        );
        // fence 内的 `##` 不切分：代码块与第一节同块。
        assert!(
            chunks[1].starts_with("## 第一节")
                && chunks[1].contains("代码块内的")
                && chunks[1].contains("fn main()"),
            "fence 内 ## 不作为标题边界"
        );
        // 末块以末节标题开头。
        assert!(chunks.last().unwrap().starts_with("## 末节"), "末节独立成块");
        // 内容完整性（重组校验）。
        let rejoined = chunks.join("\n\n");
        assert!(rejoined.contains("fn main()"), "代码内容不丢失");
        assert!(rejoined.contains("段落 299"), "超长节内容完整");
        // 空文档：无块但 frontmatter 仍可剥离。
        let (fm0, c0) = split_translation_chunks("---\ncategory: x\n---\n", 6000);
        assert_eq!(fm0, "---\ncategory: x\n---\n");
        assert!(c0.is_empty(), "空正文零块");
    }

    // ============ 14. strip_outer_fence：剥模型包裹的外层围栏（带平衡校验）============

    #[test]
    fn translate_strip_outer_fence_boundaries() {
        assert_eq!(
            strip_outer_fence("```markdown\n# T\n\ntext\n```"),
            "# T\n\ntext",
            "剥带 markdown 标记的外层围栏"
        );
        assert_eq!(
            strip_outer_fence("```\n# T\n\ntext\n```"),
            "# T\n\ntext",
            "剥裸外层围栏"
        );
        assert_eq!(
            strip_outer_fence("# 正常\n\n正文"),
            "# 正常\n\n正文",
            "无围栏原样"
        );
        // 正文自身是业务代码块（info=rust）→ 不剥（误剥会丢代码围栏）。
        assert_eq!(
            strip_outer_fence("```rust\nfn a() {}\n```"),
            "```rust\nfn a() {}\n```",
            "业务代码块不误剥"
        );
        // 多块正文被整体包裹（内部围栏配平）→ 剥外层。
        assert_eq!(
            strip_outer_fence("```\ntext\n\n```js\na\n```\n\nmore\n\n```py\nb\n```\n```"),
            "text\n\n```js\na\n```\n\nmore\n\n```py\nb\n```",
            "内部配平时剥外层包裹"
        );
        // 内部围栏不配平 → 看不出哪层是包裹，宁可不剥。
        assert_eq!(
            strip_outer_fence("```\n```js\na\n```\n\ntext\n```\n```js\nb\n```\n```"),
            "```\n```js\na\n```\n\ntext\n```\n```js\nb\n```\n```",
            "内部不配平不剥"
        );
        // 单行/空串边界。
        assert_eq!(strip_outer_fence("```"), "```");
        assert_eq!(strip_outer_fence(""), "");
    }

    // ============ 15. lang=zh / 缺省：原文直读零开销（无任务无缓存写）============

    #[tokio::test]
    async fn translate_lang_zh_is_zero_overhead_original() {
        let (root, _g) = temp_root("i18n-zh");
        let md = "# 原文\n\n正文。\n";
        write(&root.join("a.md"), md);
        let cache = std::env::temp_dir().join(format!("nexos-devdocs-i18n-zh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        // 网关指向必失败的地址：lang=zh 若误触翻译链路会暴露（这里不应有任何调用）。
        let h = translate_handler(
            &root,
            &cache,
            format!("http://127.0.0.1:{}", free_closed_port()),
            Some("tk"),
            "m",
        );
        for path in ["/api/v1/devdocs/doc/a.md", "/api/v1/devdocs/doc/a.md?lang=zh"] {
            let resp = h.handle(get_req(path)).await.unwrap();
            assert_eq!(resp.status, 200, "{path}");
            assert_eq!(resp.body["markdown"], md, "{path} 原文逐字节");
            assert!(
                resp.headers.get("X-Translation").is_none(),
                "{path} 不带翻译头"
            );
        }
        let _ = std::fs::remove_dir_all(&cache);
    }

    // ============ 16. 非法 lang → 400 ============

    #[tokio::test]
    async fn translate_unknown_lang_400() {
        let (root, _g) = temp_root("i18n-400");
        write(&root.join("a.md"), "# A\n");
        let h = DevDocsRouteHandler::with_root(Some(root));
        let resp = h
            .handle(get_req("/api/v1/devdocs/doc/a.md?lang=fr"))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
        assert!(resp.body["error"].as_str().unwrap().contains("不支持的语言"));
    }

    // ============ 17. 全链路：202 → 轮询 done → 缓存命中（X-Translation: cached）============

    #[tokio::test]
    async fn translate_full_pipeline_task_then_cached() {
        let (root, _g) = temp_root("i18n-full");
        write(&root.join("dev/guide.md"), &long_doc());
        let cache = std::env::temp_dir().join(format!("nexos-devdocs-i18n-full-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        let recorded: Arc<Mutex<Vec<RecordedGwReq>>> = Arc::new(Mutex::new(Vec::new()));
        let base = spawn_mock_gateway(gw_echo_translation, Arc::clone(&recorded));
        let h = translate_handler(&root, &cache, base, Some("sk-os-test"), "test-model");

        // —— 首次：202 + 任务视图 ——
        let r1 = h
            .handle(get_req("/api/v1/devdocs/doc/dev/guide.md?lang=en"))
            .await
            .unwrap();
        assert_eq!(r1.status, 202, "body: {r1:?}");
        assert_eq!(r1.body["status"], "running");
        assert_eq!(r1.body["lang"], "en");
        assert!(r1.body["id"].as_str().unwrap().starts_with("ddt-"));
        let chunks_total = r1.body["chunks_total"].as_u64().unwrap() as usize;
        assert!(chunks_total >= 4, "分块数进入任务视图：{chunks_total}");
        let task_id = r1.body["id"].as_str().unwrap().to_string();

        // —— 轮询到 done ——
        let settled = wait_task_settled(&h, &task_id).await;
        assert_eq!(settled["status"], "done", "body: {settled:?}");
        assert_eq!(settled["chunks_done"], settled["chunks_total"]);
        assert!(settled["finished_at"].is_number(), "finished_at 落定");

        // —— 网关侧：逐块调用，凭据 / 模型 / 分块 / 思考开关 / 预算逐项可断言 ——
        let reqs = recorded.lock().unwrap().clone();
        assert_eq!(reqs.len(), chunks_total, "每块恰好一次网关调用");
        for r in &reqs {
            assert_eq!(r.auth, "Bearer sk-os-test", "服务端凭据逐请求携带");
            assert!(r.path.contains("/api/v1/gateway/v1/chat/completions"), "{}", r.path);
            assert_eq!(r.body["model"], "test-model");
            assert_eq!(r.body["stream"], false, "非流式整包");
            // 思考开关：vLLM Qwen3 官方参数经网关透传（106 真机验证生效）。
            assert_eq!(
                r.body["chat_template_kwargs"]["enable_thinking"], false,
                "禁思考开关逐请求携带"
            );
            let sys = r.body["messages"][0]["content"].as_str().unwrap();
            assert!(sys.contains("技术文档翻译引擎"), "系统提示词在位");
            assert!(sys.contains("NodeID") && sys.contains("ZFS") && sys.contains("vLLM"), "术语表在位");
            let user = r.body["messages"][1]["content"].as_str().unwrap();
            assert!(user.chars().count() <= 6000, "user 内容即分块（≤6K）");
            // 输出预算动态：输入字符/2 + 2048（静态小值会被思考/长译文吃穿）。
            let expect_tokens =
                (sys.chars().count() + user.chars().count()) / 2 + 2048;
            assert_eq!(
                r.body["max_tokens"].as_u64().unwrap(),
                expect_tokens as u64,
                "max_tokens = 输入字符/2 + 2048"
            );
        }

        // —— 缓存文件落盘（frontmatter 原样回接）——
        let cached = std::fs::read_to_string(cache.join("en/dev/guide.md")).unwrap();
        assert!(cached.starts_with("---\ncategory: 测试\n---"), "frontmatter 不译原样回接");
        assert!(cached.contains("[T]# 测试文档"), "译文含标记（mock 回显）");
        assert!(
            cached.contains("段落 0") && cached.contains("段落 299"),
            "超长节译文完整（0..299 段全部译出）"
        );

        // —— 再取：200 + X-Translation: cached，且不再触发网关 ——
        let before = recorded.lock().unwrap().len();
        let r2 = h
            .handle(get_req("/api/v1/devdocs/doc/dev/guide.md?lang=en"))
            .await
            .unwrap();
        assert_eq!(r2.status, 200, "body: {r2:?}");
        assert_eq!(r2.headers["X-Translation"], "cached", "缓存命中响应头");
        assert!(r2.body["markdown"].as_str().unwrap().contains("[T]# 测试文档"));
        assert_eq!(recorded.lock().unwrap().len(), before, "缓存命中零网关调用");

        // —— zh-TW 独立缓存槽（en 的缓存不串用）——
        let r3 = h
            .handle(get_req("/api/v1/devdocs/doc/dev/guide.md?lang=zh-TW"))
            .await
            .unwrap();
        assert_eq!(r3.status, 202, "zh-TW 首次独立触发：{r3:?}");
        let tw_task = r3.body["id"].as_str().unwrap().to_string();
        let tw = wait_task_settled(&h, &tw_task).await;
        assert_eq!(tw["status"], "done");
        assert!(std::fs::read_to_string(cache.join("zh-TW/dev/guide.md")).is_ok(), "zh-TW 独立缓存文件");
        let _ = std::fs::remove_dir_all(&cache);
    }

    // ============ 18. running 去重：第二次请求复用任务（不重复翻译）============

    #[tokio::test]
    async fn translate_running_task_reused_no_duplicate() {
        let (root, _g) = temp_root("i18n-dedup");
        write(&root.join("a.md"), "# A\n\n正文。\n\n## B\n\n更多。\n");
        let cache = std::env::temp_dir().join(format!("nexos-devdocs-i18n-dedup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        let recorded: Arc<Mutex<Vec<RecordedGwReq>>> = Arc::new(Mutex::new(Vec::new()));
        let base = spawn_mock_gateway(gw_slow_echo, Arc::clone(&recorded));
        let h = translate_handler(&root, &cache, base, Some("tk"), "m");

        let r1 = h.handle(get_req("/api/v1/devdocs/doc/a.md?lang=en")).await.unwrap();
        assert_eq!(r1.status, 202);
        let id1 = r1.body["id"].as_str().unwrap().to_string();
        // running 窗口内（慢网关 400ms/块）立刻再请求 → 复用同一任务。
        let r2 = h.handle(get_req("/api/v1/devdocs/doc/a.md?lang=en")).await.unwrap();
        assert_eq!(r2.status, 202, "running 期间 202 复用: {r2:?}");
        assert_eq!(r2.body["id"], serde_json::json!(id1), "不重复建任务");
        assert_eq!(r2.body["status"], "running");

        let settled = wait_task_settled(&h, &id1).await;
        assert_eq!(settled["status"], "done");
        // 请求次数 == 块数（复用不产生额外网关调用）。
        let reqs = recorded.lock().unwrap().len();
        let total = settled["chunks_total"].as_u64().unwrap() as usize;
        assert_eq!(reqs, total, "网关调用数 == 块数（无重复翻译）");
        let _ = std::fs::remove_dir_all(&cache);
    }

    // ============ 19. 失效：原文 mtime 新于译文 → miss 重译（旧译不返回）============

    #[tokio::test]
    async fn translate_stale_source_mtime_retranslates() {
        let (root, _g) = temp_root("i18n-stale");
        write(&root.join("a.md"), "# A\n\n旧内容。\n");
        let cache = std::env::temp_dir().join(format!("nexos-devdocs-i18n-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        let recorded: Arc<Mutex<Vec<RecordedGwReq>>> = Arc::new(Mutex::new(Vec::new()));
        let base = spawn_mock_gateway(gw_echo_translation, Arc::clone(&recorded));
        let h = translate_handler(&root, &cache, base, Some("tk"), "m");

        // 第一次翻译完成。
        let r1 = h.handle(get_req("/api/v1/devdocs/doc/a.md?lang=en")).await.unwrap();
        let t1 = r1.body["id"].as_str().unwrap().to_string();
        assert_eq!(wait_task_settled(&h, &t1).await["status"], "done");
        let calls_after_first = recorded.lock().unwrap().len();

        // 原文更新（mtime 推到未来，绕开文件系统时间粒度）。
        let src = root.join("a.md");
        write(&src, "# A\n\n新内容：原文已更新。\n");
        let future = SystemTime::now() + Duration::from_secs(3600);
        std::fs::File::open(&src)
            .and_then(|f| f.set_modified(future))
            .unwrap();

        // 再取 → 过期判 miss：202 新任务（旧译不返回）。
        let r2 = h.handle(get_req("/api/v1/devdocs/doc/a.md?lang=en")).await.unwrap();
        assert_eq!(r2.status, 202, "过期缓存判 miss 重译: {r2:?}");
        let t2 = r2.body["id"].as_str().unwrap().to_string();
        assert_ne!(t2, t1, "新任务");
        assert_eq!(wait_task_settled(&h, &t2).await["status"], "done");
        assert!(recorded.lock().unwrap().len() > calls_after_first, "重译产生新网关调用");

        // 完成后命中新译（含新内容标记）。注：重译完成后把原文 mtime 拨回过去——
        // 本测试为触发失效把原文拨到了未来，而真实世界里原文编辑必然早于译文写入。
        std::fs::File::open(&src)
            .and_then(|f| f.set_modified(SystemTime::now() - Duration::from_secs(3600)))
            .unwrap();
        let r3 = h.handle(get_req("/api/v1/devdocs/doc/a.md?lang=en")).await.unwrap();
        assert_eq!(r3.status, 200);
        assert!(r3.body["markdown"].as_str().unwrap().contains("新内容"), "新译内容生效");
        let _ = std::fs::remove_dir_all(&cache);
    }

    // ============ 20. 无渠道 → 诚实降级：任务 error → 503 文案 → retry=1 恢复 ============

    #[tokio::test]
    async fn translate_no_channel_503_honest_degrade_and_retry() {
        let (root, _g) = temp_root("i18n-nomodel");
        write(&root.join("a.md"), "# A\n\n正文。\n");
        let cache = std::env::temp_dir().join(format!("nexos-devdocs-i18n-nomodel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        let recorded: Arc<Mutex<Vec<RecordedGwReq>>> = Arc::new(Mutex::new(Vec::new()));
        let dead = spawn_mock_gateway(gw_no_channel, Arc::clone(&recorded));
        let h = translate_handler(&root, &cache, dead, Some("tk"), "m");

        // 首次 202 → 任务以「无可用本地模型」失败（网关 404 无渠道分类）。
        let r1 = h.handle(get_req("/api/v1/devdocs/doc/a.md?lang=en")).await.unwrap();
        assert_eq!(r1.status, 202);
        let t1 = r1.body["id"].as_str().unwrap().to_string();
        let settled = wait_task_settled(&h, &t1).await;
        assert_eq!(settled["status"], "error", "body: {settled:?}");
        let err = settled["error"].as_str().unwrap();
        assert!(
            err.contains("本节点无可用本地模型") && err.contains("中文原文可用"),
            "诚实降级文案：{err}"
        );
        assert!(err.contains("无可用渠道"), "保留网关原始细节：{err}");

        // 失败后 GET ?lang → 503 + 同文案（不假翻译）。
        let r2 = h.handle(get_req("/api/v1/devdocs/doc/a.md?lang=en")).await.unwrap();
        assert_eq!(r2.status, 503, "body: {r2:?}");
        assert!(
            r2.body["error"]
                .as_str()
                .unwrap()
                .contains("本节点无可用本地模型"),
            "503 带明确文案"
        );
        assert_eq!(r2.body["task"]["id"], serde_json::json!(t1), "503 附任务视图");

        // 中文原文始终可读（对照组——降级不影响原文直读）。
        let rz = h.handle(get_req("/api/v1/devdocs/doc/a.md")).await.unwrap();
        assert_eq!(rz.status, 200);
        assert_eq!(rz.body["markdown"], "# A\n\n正文。\n");

        // retry=1：清除失败态，用「有模型」的网关（共享缓存目录的新 handler）恢复。
        let recorded2: Arc<Mutex<Vec<RecordedGwReq>>> = Arc::new(Mutex::new(Vec::new()));
        let alive = spawn_mock_gateway(gw_echo_translation, Arc::clone(&recorded2));
        let h2 = translate_handler(&root, &cache, alive, Some("tk"), "m");
        let r3 = h2
            .handle(get_req("/api/v1/devdocs/doc/a.md?lang=en&retry=1"))
            .await
            .unwrap();
        assert_eq!(r3.status, 202, "retry=1 清除失败态重新翻译: {r3:?}");
        let t3 = r3.body["id"].as_str().unwrap().to_string();
        assert_eq!(wait_task_settled(&h2, &t3).await["status"], "done");
        let r4 = h2.handle(get_req("/api/v1/devdocs/doc/a.md?lang=en")).await.unwrap();
        assert_eq!(r4.status, 200, "恢复后命中缓存");
        let _ = std::fs::remove_dir_all(&cache);
    }

    // ============ 21. 任务端点：未知 id 404；无凭据 503（配置引导）============
    #[tokio::test]
    async fn translate_task_404_and_missing_token_503() {
        let (root, _g) = temp_root("i18n-misc");
        write(&root.join("a.md"), "# A\n");
        let cache = std::env::temp_dir().join(format!("nexos-devdocs-i18n-misc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        let recorded: Arc<Mutex<Vec<RecordedGwReq>>> = Arc::new(Mutex::new(Vec::new()));
        let base = spawn_mock_gateway(gw_echo_translation, Arc::clone(&recorded));

        // 未知任务 id → 404。
        let h = translate_handler(&root, &cache, base.clone(), Some("tk"), "m");
        let r = h
            .handle(get_req("/api/v1/devdocs/translate/tasks/ddt-999"))
            .await
            .unwrap();
        assert_eq!(r.status, 404);
        assert!(r.body["error"].as_str().unwrap().contains("翻译任务不存在"));

        // 无凭据 → 503 带配置引导（不 spawn 任务）。
        let h2 = translate_handler(&root, &cache, base, None, "m");
        let r2 = h2.handle(get_req("/api/v1/devdocs/doc/a.md?lang=en")).await.unwrap();
        assert_eq!(r2.status, 503, "body: {r2:?}");
        let msg = r2.body["error"].as_str().unwrap();
        assert!(msg.contains("本节点无可用本地模型"), "统一降级文案开头：{msg}");
        assert!(msg.contains("NEXOS_DEVDOCS_GATEWAY_TOKEN"), "给出 env 配置指引");
        assert_eq!(recorded.lock().unwrap().len(), 0, "未发起任何网关调用");
        let _ = std::fs::remove_dir_all(&cache);
    }

    // ============ 23. 思考占用 → /no_think 软开关重试一次 → 成功 ============
    // （真机 2026-09-03：106 qwen3.5-9b 把翻译输出全放进思考段、content=null；
    //  官方开关 chat_template_kwargs 见全链路测试断言，本用例覆盖软开关重试路径）

    #[tokio::test]
    async fn translate_thinking_occupied_retries_with_soft_switch() {
        let (root, _g) = temp_root("i18n-think");
        write(&root.join("a.md"), "# A\n\n正文一段。\n"); // 单块文档
        let cache = std::env::temp_dir().join(format!("nexos-devdocs-i18n-think-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        let recorded: Arc<Mutex<Vec<RecordedGwReq>>> = Arc::new(Mutex::new(Vec::new()));
        let base = spawn_mock_gateway(gw_thinking_first, Arc::clone(&recorded));
        let h = translate_handler(&root, &cache, base, Some("tk"), "m");

        let r1 = h.handle(get_req("/api/v1/devdocs/doc/a.md?lang=en")).await.unwrap();
        assert_eq!(r1.status, 202);
        let task_id = r1.body["id"].as_str().unwrap().to_string();
        let settled = wait_task_settled(&h, &task_id).await;
        assert_eq!(settled["status"], "done", "重试后成功: {settled:?}");

        // 恰好两次调用：首发思考占用 → 软开关重试成功。
        let reqs = recorded.lock().unwrap().clone();
        assert_eq!(reqs.len(), 2, "首发 + 软开关重试各一次");
        // 重试请求：user 内容尾带 /no_think，且禁思考官方开关仍在位。
        let retry_user = reqs[1].body["messages"][1]["content"].as_str().unwrap();
        assert!(retry_user.ends_with("/no_think"), "软开关追加：{retry_user:?}");
        assert_eq!(reqs[1].body["chat_template_kwargs"]["enable_thinking"], false);
        // 任务日志含区分文案（前端可见重试动作）。
        let log = settled["log"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(log.contains("思考段占用"), "日志区分思考占用：{log}");

        // 缓存命中（重试译文为回显标记）。
        let r2 = h.handle(get_req("/api/v1/devdocs/doc/a.md?lang=en")).await.unwrap();
        assert_eq!(r2.status, 200);
        assert!(r2.body["markdown"].as_str().unwrap().contains("[T]# A"));
        let _ = std::fs::remove_dir_all(&cache);
    }

    // ============ 24. 思考占用且重试仍失败 → 终态 error 文案区分 ============

    #[tokio::test]
    async fn translate_thinking_occupied_retry_exhausted_error() {
        let (root, _g) = temp_root("i18n-think2");
        write(&root.join("a.md"), "# A\n\n正文。\n");
        let cache = std::env::temp_dir().join(format!("nexos-devdocs-i18n-think2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cache);
        let recorded: Arc<Mutex<Vec<RecordedGwReq>>> = Arc::new(Mutex::new(Vec::new()));
        let base = spawn_mock_gateway(gw_always_thinking, Arc::clone(&recorded));
        let h = translate_handler(&root, &cache, base, Some("tk"), "m");

        let r1 = h.handle(get_req("/api/v1/devdocs/doc/a.md?lang=en")).await.unwrap();
        assert_eq!(r1.status, 202);
        let task_id = r1.body["id"].as_str().unwrap().to_string();
        let settled = wait_task_settled(&h, &task_id).await;
        assert_eq!(settled["status"], "error", "重试做尽落 error: {settled:?}");
        let err = settled["error"].as_str().unwrap();
        assert!(
            err.contains("思考段占用") && err.contains("重试一次"),
            "终态文案区分思考占用：{err}"
        );
        assert_eq!(recorded.lock().unwrap().len(), 2, "恰好两次（首发+重试），不无限重试");
        // 不写缓存：后续 GET 仍 503（失败态），原文不受影响。
        let r2 = h.handle(get_req("/api/v1/devdocs/doc/a.md?lang=en")).await.unwrap();
        assert_eq!(r2.status, 503);
        let rz = h.handle(get_req("/api/v1/devdocs/doc/a.md")).await.unwrap();
        assert_eq!(rz.status, 200, "中文原文不受影响");
        let _ = std::fs::remove_dir_all(&cache);
    }
}
