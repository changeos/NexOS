//! `AgentHubRouteHandler` —— 「Agent 集合」桌面应用的 HTTP 适配器：
//! 常用 AI coding agent（OpenCode / OpenClaw / Claude Code / Codex / Gemini CLI /
//! Aider / Goose …）的目录浏览、一键安装/卸载任务管理与工具链探测。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/agenthub/*`）翻译为 agent 目录服务 +
//! 真实安装子进程 spawn（fire-and-forget 后台任务）。这是 OS「Agent 集合」
//! 桌面应用的后端 REST 入口。与应用中心（app_store，NexOS 内置模块）和
//! agent 协调组件（agent_coord，IM @ 定向投递）均不重叠：本组件管的是
//! **装哪些 AI agent CLI 到本机**。
//!
//! # 安装渠道（install_type → 真实命令）
//!
//! | install_type | 安装命令 | 卸载命令 | 说明 |
//! |---|---|---|---|
//! | `npm`  | `[sudo] npm install -g <target>` | `[sudo] npm uninstall -g <target>` | 系统级 npm 前缀不可写时自动加 sudo（env `NEXOS_AGENTHUB_NPM_SUDO`=always/never 覆盖）|
//! | `script` | `bash -c "curl -fsSL <url> \| bash"` | 不支持（HTTP 400）| 官方 curl 安装脚本 |
//! | `uv`   | `uv tool install <target>` | `uv tool uninstall <target>` | Python 工具隔离环境 |
//! | `cargo` | `cargo install <target>` | `cargo uninstall <target>` | Rust 生态 |
//!
//! 命令构造为纯函数（[`build_install_cmd`] / [`build_uninstall_cmd`]，可单测
//! 不真跑）；安装/卸载为 tokio::spawn 后台任务（请求立即返回 task id，
//! 进程退出码回写任务状态 + log_tail 尾 10 行），spawn 失败 / 退出非 0 降级
//! `failed`，绝不 panic。
//!
//! # 已安装探测
//!
//! `sh -c "command -v <check_binary>"`（spawn_blocking）逐目录条目探测——
//! `check_binary` 经 [`is_safe_binary_name`] 白名单校验（字母数字 `.` `_` `-`），
//! 杜绝 shell 注入。工具链探测（node/npm/uv/cargo/curl）带 3s 超时。
//! 用户级安装目录兜底：`~/.local/bin`、`~/.cargo/bin`、
//! `~/.nvm/versions/node/<ver>/bin`（nvm 装的 node/npm 与 npm -g 前缀，见
//! [`resolve_bin_in`] / [`nvm_bin_dirs`]）——探测命中即用完整路径调用。
//!
//! # 工具链手动安装（子模块 [`agenthub_toolchain`]）
//!
//! `POST /api/v1/agenthub/toolchain/install`（admin，body `{name: "node"|"uv"|
//! "cargo"}`，node 覆盖 node+npm）→ `202 {task_id}` 异步任务（进程内任务表 +
//! 环形日志 200 行，轮询 `GET /agenthub/toolchain/install/tasks/:id`）。
//! 一律**用户态安装，无 sudo/apt**：node 走 nvm（ghfast.top 镜像优先 +
//! npmmirror node 二进制镜像，装到 ~/.nvm）；uv 走官方脚本（回退 ghfast
//! Releases 代理）；cargo 走 rustup（清华 TUNA dist 镜像）。已装探测命中 →
//! 任务直接 done；重复任务 409。详见 `agenthub_toolchain.rs` 模块头与
//! docs/AGENT_HUB.md。
//!
//! # Web 界面 agent（「打开界面」能力）
//!
//! 实测确认带 Web UI 的 agent（首期 OpenCode）在目录条目上标注可选
//! `web` 描述符（[`AgentWebDesc`]：start_cmd / port / url_path / note）——
//! **未标注的 agent 前端不显示「打开界面」按钮，诚实不猜**。三个端点管理
//! 其后台服务进程（进程表 agentId→{pid, port, started_at, 日志环形 100 行}）：
//! `POST /agenthub/web/:agentId/start`（admin，spawn `start_cmd`（argv[0] 经
//! [`resolve_bin`] 解析绝对路径）→ ≤15s 端口就绪轮询 → 200 `{url, pid}`；
//! 已在跑幂等返回；端口被占但表丢失 → 按端口探测兜底重建表（os-api 重启后
//! 子进程存活的诚实恢复））；`POST .../stop`（admin，SIGTERM→SIGKILL / 恢复
//! 条目 fuser 按端口杀）；`GET .../status`（公开，`{running, url, started_at,
//! log_tail}`）。URL 用请求 Host 头推导（去 API 端口换服务端口——跨机访问
//! 即节点 IP，provisioning clone_url 地址链同款先例：Host 头 →
//! `NEXOS_GIT_ADVERTISE_HOST` → 127.0.0.1）。外部命令经 [`WebLauncher`]
//! 抽象（生产真实 spawn + TCP 探测，测试注入 mock）。
//!
//! # 持久化
//!
//! 用户发布的自定义 agent：JSON 文件（env `NEXOS_AGENTHUB_FILE`，缺省
//! `/tank/os-data/agenthub.json`），原子写（先 `.tmp` 再 rename，update.rs
//! 同款）；目录不存在自动创建；读取缺失/损坏 → 空态降级不阻塞启动。
//! 任务列表内存态（重启即清，同 app_store 惯例），上限 100 条。
//!
//! # 鉴权
//!
//! 读（目录/探测/任务/统计/Web 状态）公开；写（安装/卸载/发布/删除/Web
//! start/stop）admin Bearer（网关 `NEXOS_ADMIN_TOKEN`）。
//!
//! # 路由表（16 条，component="agenthub"；2 条由 [`agenthub_toolchain`] 提供，
//! 3 条 Web 界面管理）
//!
//! | method | path                                | 动作 |
//! |--------|-------------------------------------|------|
//! | GET    | `/api/v1/agenthub/agents`           | 目录（预置 + 自定义，含 installed 探测）|
//! | GET    | `/api/v1/agenthub/agents/:id`       | 单 agent 详情 |
//! | GET    | `/api/v1/agenthub/installed`        | 已安装列表（command -v 探测）|
//! | GET    | `/api/v1/agenthub/toolchains`       | 工具链可用性（node/npm/uv/cargo/curl）|
//! | POST   | `/api/v1/agenthub/install`          | 一键安装（admin，后台任务）|
//! | POST   | `/api/v1/agenthub/uninstall`        | 卸载（admin；script 渠道 400）|
//! | GET    | `/api/v1/agenthub/tasks`            | 任务列表 |
//! | GET    | `/api/v1/agenthub/tasks/:id`        | 任务详情（含 log_tail）|
//! | POST   | `/api/v1/agenthub/publish`          | 发布自定义 agent（admin，持久化）|
//! | DELETE | `/api/v1/agenthub/published/:id`    | 删自定义 agent（admin，仅自定义可删）|
//! | GET    | `/api/v1/agenthub/stats`            | 聚合统计 |
//! | POST   | `/api/v1/agenthub/toolchain/install`            | 工具链手动安装（admin，202 异步任务）|
//! | GET    | `/api/v1/agenthub/toolchain/install/tasks/:id`  | 工具链安装任务详情（含环形日志）|
//! | POST   | `/api/v1/agenthub/web/:agentId/start`           | 启动 agent Web 服务（admin，返回 URL）|
//! | POST   | `/api/v1/agenthub/web/:agentId/stop`            | 停止 agent Web 服务（admin）|
//! | GET    | `/api/v1/agenthub/web/:agentId/status`          | agent Web 服务状态（公开）|

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::agenthub_toolchain;
use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// 常量与 env
// ----------------------------------------------------------------------------

/// 组件名（路由注册用）。
const COMPONENT: &str = "agenthub";

/// 自定义 agent 持久化文件 env（缺省 [`DEFAULT_STATE_FILE`]）。
const ENV_STATE_FILE: &str = "NEXOS_AGENTHUB_FILE";

/// 持久化缺省路径（/tank/os-data 部署布局）。
pub const DEFAULT_STATE_FILE: &str = "/tank/os-data/agenthub.json";

/// npm 全局安装 sudo 策略 env：`always` / `never` / 其他（缺省 auto 探测）。
const ENV_NPM_SUDO: &str = "NEXOS_AGENTHUB_NPM_SUDO";

/// 任务列表内存上限（超出裁最旧）。
const MAX_TASKS: usize = 100;

/// 工具链探测单命令超时。
const TOOLCHAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// 合法安装渠道。
pub const INSTALL_TYPES: [&str; 4] = ["npm", "script", "uv", "cargo"];

/// OpenCode Web 服务固定端口（实测：`opencode serve --port` 缺省 0=随机端口，
/// 必须显式固定；4096 为 opencode 官方文档 serve 示例常用端口）。
pub const OPENCODE_WEB_PORT: u16 = 4096;

/// Web 服务日志环形上限（行）。
const WEB_LOG_MAX_LINES: usize = 100;

/// Web 服务端口就绪探测总时限（实测 opencode serve 本地 ~1s 内开始监听，
/// 15s 宽裕上限；就绪判定 = TCP 连通 + 进程仍存活）。
const WEB_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// 端口就绪轮询间隔。
const WEB_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// 通告地址 env（URL 推导回退链：Host 头 → 此 env → 127.0.0.1；与
/// provisioning clone_url 地址链同款先例）。
const ENV_ADVERTISE_HOST: &str = "NEXOS_GIT_ADVERTISE_HOST";

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 目录条目（预置 + 用户发布；`installed` 为响应时探测合并，不持久化）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogAgent {
    /// 唯一 id（预置为固定 slug，自定义为 `custom-<seq>`）。
    pub id: String,
    /// 显示名。
    pub name: String,
    /// 简介。
    pub description: String,
    /// 分类：coding（编码代理）/ assistant（助手）/ custom。
    pub category: String,
    /// 图标 emoji。
    pub icon: String,
    /// 来源：`preset` / `user`。
    pub source: String,
    /// 安装渠道：npm / script / uv / cargo。
    pub install_type: String,
    /// 安装目标：npm 包名 / 脚本 URL / uv 包名 / crate 名。
    pub install_target: String,
    /// 安装后可执行文件名（installed 探测用，command -v）。
    pub check_binary: String,
    /// 主页 URL。
    pub homepage: String,
    /// 发布者。
    pub publisher: String,
    /// 标签。
    pub tags: Vec<String>,
    /// 是否已安装（探测合并，缺省 false）。
    #[serde(default)]
    pub installed: bool,
    /// Web 界面描述符（**仅对实测确认有 Web UI 的 agent 标注**，首期 OpenCode；
    /// 未标注 → 前端无「打开界面」按钮。旧持久化文件缺此键 → None；None
    /// 序列化跳过不落键）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web: Option<AgentWebDesc>,
}

/// agent Web 界面描述符：声明「装好后可一键起服务并打开网页」所需的一切。
/// 由代码常量标注（预置目录），不经 publish API 注入——避免把任意命令行
/// 混进目录数据（诚实不猜：没实测过的 agent 不标）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentWebDesc {
    /// 启动命令（argv；argv[0] spawn 前经 [`resolve_bin`] 解析用户级安装位置
    /// 的绝对路径——npm -g 装的 CLI 常不在服务 PATH 上）。
    pub start_cmd: Vec<String>,
    /// 固定服务端口（探测就绪 / 停止 / 状态恢复都按它）。
    pub port: u16,
    /// Web UI 路径（含前导 `/`；OpenCode 根路径即界面）。
    pub url_path: String,
    /// 备注（鉴权形态等实测记录，前端 tooltip 展示）。
    pub note: String,
}

/// 安装/卸载任务。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentTask {
    /// 任务 id（`task-<seq>`）。
    pub id: String,
    /// 关联 agent id。
    pub agent_id: String,
    /// agent 显示名（冗余）。
    pub agent_name: String,
    /// `install` / `uninstall`。
    pub action: String,
    /// 安装渠道。
    pub install_type: String,
    /// `pending` / `running` / `completed` / `failed`。
    pub status: String,
    /// 进程 pid（运行中）。
    pub pid: Option<u32>,
    /// 失败原因。
    pub error: Option<String>,
    /// 输出尾部（≤10 行）。
    pub log_tail: Option<String>,
    /// 创建时间（ISO 8601）。
    pub created_at: String,
}

/// 已安装条目（GET /installed 元素）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstalledAgent {
    pub id: String,
    pub name: String,
    pub binary: String,
}

/// 工具链条目（GET /toolchains 元素）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolchainInfo {
    /// 工具名：node / npm / uv / cargo / curl。
    pub name: String,
    /// 是否可用。
    pub available: bool,
    /// 版本首行（探测失败为空）。
    pub version: String,
}

/// `GET /stats` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHubStats {
    /// 目录总数（预置 + 自定义）。
    pub total_agents: usize,
    /// 已安装数（探测）。
    pub installed: usize,
    /// 可用工具链数。
    pub toolchains_ready: usize,
    /// 任务总数（内存态）。
    pub tasks: usize,
}

/// 安装/卸载请求体。
#[derive(Debug, Deserialize)]
struct ActionBody {
    agent_id: String,
}

/// 发布请求体。
#[derive(Debug, Deserialize)]
struct PublishBody {
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    category: String,
    #[serde(default)]
    install_type: String,
    #[serde(default)]
    install_target: String,
    #[serde(default)]
    check_binary: String,
    #[serde(default)]
    homepage: String,
    #[serde(default)]
    tags: Vec<String>,
}

/// 持久化状态（自定义 agent 列表）。
#[derive(Debug, Default, Serialize, Deserialize)]
struct PersistState {
    agents: Vec<CatalogAgent>,
    /// 自定义 id 序号（单调递增，防重启后 id 撞车）。
    #[serde(default)]
    seq: u64,
}

// ----------------------------------------------------------------------------
// 纯函数（命令构造 / 校验，可单测不执行）
// ----------------------------------------------------------------------------

/// 构造安装命令。未知渠道返回空 Vec（caller 置任务 failed，不 spawn）。
///
/// - npm：`npm install -g <target>`（`npm_sudo` 时前置 `sudo`——系统级 npm
///   前缀不可写场景，stdin 已 null，sudo 需密码时立即失败不挂起）；
/// - script：`bash -c "curl -fsSL <url> | bash"`（官方安装脚本）；
/// - uv：`uv tool install <target>`；
/// - cargo：`cargo install <target>`。
#[must_use]
pub fn build_install_cmd(install_type: &str, target: &str, npm_sudo: bool) -> Vec<String> {
    match install_type {
        "npm" => {
            if npm_sudo {
                vec![
                    "sudo".into(),
                    "npm".into(),
                    "install".into(),
                    "-g".into(),
                    target.into(),
                ]
            } else {
                vec!["npm".into(), "install".into(), "-g".into(), target.into()]
            }
        }
        "script" => vec![
            "bash".into(),
            "-c".into(),
            format!("curl -fsSL {target} | bash"),
        ],
        "uv" => vec!["uv".into(), "tool".into(), "install".into(), target.into()],
        "cargo" => vec!["cargo".into(), "install".into(), target.into()],
        _ => Vec::new(),
    }
}

/// 构造卸载命令。script 渠道与未知渠道返回空 Vec（HTTP 层前置 400，不会走到 spawn）。
#[must_use]
pub fn build_uninstall_cmd(install_type: &str, target: &str, npm_sudo: bool) -> Vec<String> {
    match install_type {
        "npm" => {
            if npm_sudo {
                vec![
                    "sudo".into(),
                    "npm".into(),
                    "uninstall".into(),
                    "-g".into(),
                    target.into(),
                ]
            } else {
                vec!["npm".into(), "uninstall".into(), "-g".into(), target.into()]
            }
        }
        "uv" => vec![
            "uv".into(),
            "tool".into(),
            "uninstall".into(),
            target.into(),
        ],
        "cargo" => vec!["cargo".into(), "uninstall".into(), target.into()],
        _ => Vec::new(),
    }
}

/// 可执行文件名白名单校验（防 shell 注入：仅字母数字与 `.` `_` `-`）。
#[must_use]
pub fn is_safe_binary_name(bin: &str) -> bool {
    !bin.is_empty()
        && bin.len() <= 64
        && bin
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// 安装目标校验：script 必须是 http(s) URL，其余渠道非空即可。
#[must_use]
pub fn is_valid_target(install_type: &str, target: &str) -> bool {
    let t = target.trim();
    if t.is_empty() || t.len() > 512 || t.chars().any(|c| c.is_control()) {
        return false;
    }
    if install_type == "script" {
        t.starts_with("https://") || t.starts_with("http://")
    } else {
        !t.contains(char::is_whitespace)
    }
}

/// 返回预置 agent 目录（常用 AI coding agent，9 条）。
#[must_use]
pub fn preset_agents() -> Vec<CatalogAgent> {
    let a = |id: &str,
             name: &str,
             desc: &str,
             cat: &str,
             icon: &str,
             itype: &str,
             target: &str,
             bin: &str,
             home: &str,
             tags: &[&str]| CatalogAgent {
        id: id.into(),
        name: name.into(),
        description: desc.into(),
        category: cat.into(),
        icon: icon.into(),
        source: "preset".into(),
        install_type: itype.into(),
        install_target: target.into(),
        check_binary: bin.into(),
        homepage: home.into(),
        publisher: "NexOS 官方".into(),
        tags: tags.iter().map(|s| (*s).into()).collect(),
        installed: false,
        web: None,
    };
    let mut agents = vec![
        a(
            "opencode",
            "OpenCode",
            "开源终端 AI 编码代理（SST）：TUI 会话 / LSP / 多模型",
            "coding",
            "🧑‍💻",
            "npm",
            "opencode-ai",
            "opencode",
            "https://opencode.ai",
            &["tui", "opensource", "coding"],
        ),
        a(
            "openclaw",
            "OpenClaw",
            "开源个人 AI 助手网关：接入 IM / 多模型 / 技能扩展",
            "assistant",
            "🐾",
            "npm",
            "openclaw",
            "openclaw",
            "https://openclaw.ai",
            &["assistant", "opensource", "gateway"],
        ),
        a(
            "claude-code",
            "Claude Code",
            "Anthropic 官方编码 agent CLI：终端结对 / 工具调用 / MCP",
            "coding",
            "🤖",
            "npm",
            "@anthropic-ai/claude-code",
            "claude",
            "https://claude.com/product/claude-code",
            &["anthropic", "coding", "official"],
        ),
        a(
            "codex",
            "Codex CLI",
            "OpenAI 官方编码 agent CLI：终端会话 / 沙箱执行",
            "coding",
            "✨",
            "npm",
            "@openai/codex",
            "codex",
            "https://developers.openai.com/codex/cli",
            &["openai", "coding", "official"],
        ),
        a(
            "gemini-cli",
            "Gemini CLI",
            "Google 官方编码 agent CLI：Gemini 模型终端会话 / MCP",
            "coding",
            "💎",
            "npm",
            "@google/gemini-cli",
            "gemini",
            "https://github.com/google-gemini/gemini-cli",
            &["google", "coding", "official"],
        ),
        a(
            "qwen-code",
            "Qwen Code",
            "通义千问编码 agent CLI：Qwen 模型终端会话（Gemini CLI 血统）",
            "coding",
            "🀄",
            "npm",
            "@qwen-code/qwen-code",
            "qwen",
            "https://github.com/QwenLM/qwen-code",
            &["qwen", "coding"],
        ),
        a(
            "aider",
            "Aider",
            "终端 AI 结对编程：git 感知编辑 / 多模型 / 仓库地图",
            "coding",
            "🤝",
            "uv",
            "aider-chat",
            "aider",
            "https://aider.chat",
            &["python", "git", "pair"],
        ),
        a(
            "goose",
            "Goose",
            "Block 开源 AI agent：可扩展 MCP 工具 / 自动化工作流",
            "assistant",
            "🪿",
            "script",
            "https://github.com/block/goose/releases/download/stable/download_cli.sh",
            "goose",
            "https://block.github.io/goose",
            &["rust", "mcp", "automation"],
        ),
        a(
            "crush",
            "Crush",
            "Charm 出品 TUI AI 编码 agent：多模型 / LSP / 美丽终端 UI",
            "coding",
            "🦀",
            "script",
            "https://crush.charm.sh/install.sh",
            "crush",
            "https://crush.charm.sh",
            &["charm", "tui", "coding"],
        ),
    ];
    // —— OpenCode Web 界面标注（实测 2026-09-02，opencode-ai 1.18.26，详见
    // docs/AGENT_HUB.md「Web 界面 agent」节）——
    // `opencode serve --port 4096 --hostname 0.0.0.0` 起 headless HTTP 服务，
    // Web UI 直接服务在根路径 `/`（200 HTML，无重定向）；`--port` 缺省 0=
    // 随机端口故必须显式固定；`--hostname` 缺省 127.0.0.1，跨机（节点 IP）
    // 访问需 0.0.0.0；无 token（stderr 明确警告 `OPENCODE_SERVER_PASSWORD
    // is not set; server is unsecured`——OpenCode 自身经该 env 做鉴权，本
    // 组件不注入，如实记录）；端口被占时启动即退（ServeError）。
    for agent in agents.iter_mut() {
        if agent.id == "opencode" {
            agent.web = Some(AgentWebDesc {
                start_cmd: vec![
                    "opencode".into(),
                    "serve".into(),
                    "--port".into(),
                    OPENCODE_WEB_PORT.to_string(),
                    "--hostname".into(),
                    "0.0.0.0".into(),
                ],
                port: OPENCODE_WEB_PORT,
                url_path: "/".into(),
                note: "OpenCode Web UI：serve 起 HTTP 服务，根路径即界面；默认无鉴权（OPENCODE_SERVER_PASSWORD 未设时服务开放，OpenCode 自身行为，按需自行配置该 env）".into(),
            });
        }
    }
    agents
}

// ----------------------------------------------------------------------------
// 探测（阻塞辅助，caller 包 spawn_blocking）
// ----------------------------------------------------------------------------

/// 逐二进制名探测是否在 PATH 上。`command -v` 之外显式探测用户级安装目录
/// （`~/.local/bin`：npm 用户级前缀 / uv tool；`~/.cargo/bin`：cargo install；
/// `~/.nvm/versions/node/*/bin`：nvm 装的 node/npm 及 npm -g 前缀——systemd
/// 服务 PATH 可能不含这些目录，靠 sh -c command -v 会漏判）。
fn detect_binaries_blocking(bins: &[String]) -> std::collections::HashSet<String> {
    let home = std::env::var("HOME").unwrap_or_default();
    detect_binaries_in(&home, bins)
}

/// [`detect_binaries_blocking`] 的可注入根目录版本（测试用临时 HOME）。
fn detect_binaries_in(home: &str, bins: &[String]) -> std::collections::HashSet<String> {
    let mut user_bins: Vec<std::path::PathBuf> = if home.is_empty() {
        Vec::new()
    } else {
        [".local/bin", ".cargo/bin"]
            .iter()
            .map(|d| std::path::Path::new(home).join(d))
            .collect()
    };
    user_bins.extend(nvm_bin_dirs(home));
    let mut found = std::collections::HashSet::new();
    for bin in bins {
        if !is_safe_binary_name(bin) {
            continue;
        }
        let in_user_dir = user_bins.iter().any(|d| d.join(bin).exists());
        let on_path = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("command -v {bin} >/dev/null 2>&1"))
            .status()
            .is_ok_and(|s| s.success());
        if in_user_dir || on_path {
            found.insert(bin.clone());
        }
    }
    found
}

/// `~/.nvm/versions/node/<ver>/bin` 目录列表（版本号降序——最高版本优先；
/// 目录名解析不出版本号的排最后）。nvm 用户态安装的 node/npm/npm -g 装的
/// agent CLI 都落在这里（AgentHub 工具链安装器 + 探测兜底共用）。
#[must_use]
pub fn nvm_bin_dirs(home: &str) -> Vec<std::path::PathBuf> {
    let versions_root = std::path::Path::new(home)
        .join(".nvm")
        .join("versions")
        .join("node");
    let Ok(rd) = std::fs::read_dir(&versions_root) else {
        return Vec::new();
    };
    let mut dirs: Vec<(Vec<u64>, std::path::PathBuf)> = rd
        .flatten()
        .filter(|e| e.path().join("bin").is_dir())
        .map(|e| {
            // 目录名 "v20.11.1" → 版本键 [20, 11, 1]（数值比较，避免字典序
            // "v10" < "v9" 的错排；解析失败 → 空键排最后）
            let key: Vec<u64> = e
                .file_name()
                .to_string_lossy()
                .trim_start_matches('v')
                .split('.')
                .filter_map(|seg| seg.parse().ok())
                .collect();
            (key, e.path().join("bin"))
        })
        .collect();
    dirs.sort_by(|a, b| b.0.cmp(&a.0));
    dirs.into_iter().map(|(_, p)| p).collect()
}

/// 解析可执行文件路径（纯函数）：用户级 bin 目录（`~/.local/bin`、`~/.cargo/bin`、
/// `~/.nvm/versions/node/<ver>/bin` 最高版本优先）存在即返回绝对路径，否则原样
/// 返回名字（交还 PATH 解析）。
///
/// systemd 服务 PATH 常不含用户级 bin（uv/cargo/npm 用户前缀装在那里；nvm
/// 装的 node/npm 只在 `~/.nvm/versions/node/*/bin`——AgentHub 工具链安装器
/// 装完即被这里兜底命中），spawn 前与工具链探测都必须显式解析，否则误判
/// 不可用 / spawn 失败。
#[must_use]
pub fn resolve_bin_in(home: &str, name: &str) -> String {
    if home.is_empty() || !is_safe_binary_name(name) {
        return name.to_string();
    }
    for d in [".local/bin", ".cargo/bin"] {
        let p = std::path::Path::new(home).join(d).join(name);
        if p.exists() {
            return p.to_string_lossy().into_owned();
        }
    }
    for d in nvm_bin_dirs(home) {
        let p = d.join(name);
        if p.exists() {
            return p.to_string_lossy().into_owned();
        }
    }
    name.to_string()
}

/// [`resolve_bin_in`] 的 env 入口（HOME 取运行环境）。
fn resolve_bin(name: &str) -> String {
    resolve_bin_in(&std::env::var("HOME").unwrap_or_default(), name)
}

/// npm 全局前缀是否不可写（需要 sudo）：探测 `npm config get prefix` 目录写权限。
fn npm_needs_sudo_blocking() -> bool {
    let Ok(out) = std::process::Command::new("npm")
        .args(["config", "get", "prefix"])
        .output()
    else {
        return false; // npm 不存在：不加 sudo，安装任务自会失败并留日志
    };
    if !out.status.success() {
        return false;
    }
    let prefix = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if prefix.is_empty() {
        return false;
    }
    let dir = std::path::Path::new(&prefix)
        .join("lib")
        .join("node_modules");
    let probe_dir = if dir.exists() {
        dir
    } else {
        std::path::PathBuf::from(&prefix)
    };
    let probe_file = probe_dir.join(".nexos-agenthub-write-probe");
    if std::fs::write(&probe_file, b"").is_ok() {
        let _ = std::fs::remove_file(&probe_file);
        return false;
    }
    true
}

/// 解析 npm sudo 策略 env。
fn npm_sudo_policy() -> Option<bool> {
    match std::env::var(ENV_NPM_SUDO).as_deref() {
        Ok("always") => Some(true),
        Ok("never") => Some(false),
        _ => None, // auto：安装时探测
    }
}

// ----------------------------------------------------------------------------
// 持久化（原子写，update.rs 同款）
// ----------------------------------------------------------------------------

/// 原子写 JSON（先 `.tmp` 再 rename；父目录不存在自动创建）。
fn persist_state_to(path: &str, st: &PersistState) -> Result<(), String> {
    let p = std::path::Path::new(path);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建状态目录失败 {dir:?}: {e}"))?;
    }
    let tmp = format!("{path}.tmp");
    let body = serde_json::to_string_pretty(st).map_err(|e| format!("状态序列化失败: {e}"))?;
    std::fs::write(&tmp, body).map_err(|e| format!("写临时状态失败 {tmp}: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("原子替换状态失败 {path}: {e}"))
}

/// 读回状态（缺失/损坏 → 缺省空态，不报错）。
fn load_state_from(path: &str) -> PersistState {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => PersistState::default(),
    }
}

// ----------------------------------------------------------------------------
// Web 界面服务管理（进程表 + 启动器抽象 + URL 推导）
// ----------------------------------------------------------------------------

/// 运行中的 agent Web 服务进程（进程表条目；os-api 重启即失，状态查询/启动
/// 时按端口探测兜底重建——子进程不随 os-api 退出而亡，诚实恢复）。
#[derive(Debug, Clone)]
struct WebProc {
    /// 子进程 pid（重启恢复的条目为 None——表丢了 pid 无从知晓）。
    pid: Option<u32>,
    port: u16,
    /// 启动时间（ISO 8601；恢复条目为 None）。
    started_at: Option<String>,
    /// stdout/stderr 环形日志（≤[`WEB_LOG_MAX_LINES`] 行）。
    log: Vec<String>,
}

/// Web 服务进程启动器抽象：生产 [`ProcessWebLauncher`] 真实 spawn + TCP
/// 探测；测试注入 mock（fake 进程/端口探针，绝不真跑 opencode——
/// ToolchainExecutor 同款铁律）。
pub trait WebLauncher: Send + Sync {
    /// 后台 spawn `argv`（生产实现先把 argv[0] 经 [`resolve_bin`] 解析为
    /// 用户级安装位置的绝对路径再 spawn——npm -g 装的 CLI 常不在服务
    /// PATH 上），返回 pid；stdout/stderr 逐行经 `on_line` 回调进环形日志
    /// （进程退出也回调一行 `[exit]` 标记）。
    fn spawn(
        &self,
        argv: &[String],
        on_line: Arc<dyn Fn(&str) + Send + Sync>,
    ) -> Result<u32, String>;
    /// 终止进程（SIGTERM → ≤3s 轮询 → SIGKILL 兜底）。
    fn kill(&self, pid: u32) -> Result<(), String>;
    /// 按端口终止占用进程（pid 丢失的重启恢复条目用：`fuser -k <port>/tcp`）。
    fn kill_port(&self, port: u16) -> Result<(), String>;
    /// TCP 探测端口是否可连（127.0.0.1:port，500ms 超时）。
    fn port_open(&self, port: u16) -> bool;
    /// 进程是否仍存活（`kill -0`）。
    fn pid_alive(&self, pid: u32) -> bool;
}

/// 生产启动器：std::process 真实 spawn（stdout/stderr 读者线程 + 收尸线程
/// 防僵尸），`kill` 二进制发信号，`TcpStream::connect_timeout` 探端口。
struct ProcessWebLauncher;

impl ProcessWebLauncher {
    /// 读管道逐行回调（BufRead::lines；EOF/错误即止）。
    fn pipe_lines<R: std::io::Read>(r: R, sink: &Arc<dyn Fn(&str) + Send + Sync>) {
        use std::io::BufRead;
        for line in std::io::BufReader::new(r).lines() {
            match line {
                Ok(l) => sink(&l),
                Err(_) => break,
            }
        }
    }
}

impl WebLauncher for ProcessWebLauncher {
    fn spawn(
        &self,
        argv: &[String],
        on_line: Arc<dyn Fn(&str) + Send + Sync>,
    ) -> Result<u32, String> {
        let Some((program, rest)) = argv.split_first() else {
            return Err("start_cmd 为空".into());
        };
        // 同安装探测口径解析绝对路径（~/.local/bin、~/.nvm/…/bin 兜底）
        let program = resolve_bin(program);
        let mut cmd = std::process::Command::new(&program);
        cmd.args(rest)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("启动 {program} 失败（可能未安装）: {e}"))?;
        let pid = child.id();
        if let Some(out) = child.stdout.take() {
            let sink = Arc::clone(&on_line);
            std::thread::spawn(move || Self::pipe_lines(out, &sink));
        }
        if let Some(err) = child.stderr.take() {
            let sink = Arc::clone(&on_line);
            std::thread::spawn(move || Self::pipe_lines(err, &sink));
        }
        // 收尸线程：wait 回收（防僵尸）+ 退出标记行（进程早夭时让就绪轮询
        // 立即失败，而不是傻等满 15s）
        std::thread::spawn(move || {
            let code = child.wait().ok().and_then(|s| s.code());
            on_line(&format!("[exit] 进程已退出（退出码 {code:?}）"));
        });
        Ok(pid)
    }

    fn kill(&self, pid: u32) -> Result<(), String> {
        let pid_s = pid.to_string();
        let term = std::process::Command::new("kill")
            .arg(&pid_s)
            .status()
            .map_err(|e| format!("kill {pid} 失败: {e}"))?;
        if !term.success() {
            return Ok(()); // ESRCH：早已退出
        }
        for _ in 0..30 {
            if !self.pid_alive(pid) {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        let _ = std::process::Command::new("kill")
            .arg("-9")
            .arg(&pid_s)
            .status();
        Ok(())
    }

    fn kill_port(&self, port: u16) -> Result<(), String> {
        // 端口号是 u16 常量派生，无注入面；fuser 缺失时如实报错
        let st = std::process::Command::new("fuser")
            .arg("-k")
            .arg(format!("{port}/tcp"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map_err(|e| format!("fuser 不可用（无法按端口终止恢复条目）: {e}"))?;
        if st.success() {
            Ok(())
        } else {
            Err(format!("fuser -k {port}/tcp 未命中进程或失败"))
        }
    }

    fn port_open(&self, port: u16) -> bool {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500)).is_ok()
    }

    fn pid_alive(&self, pid: u32) -> bool {
        std::process::Command::new("kill")
            .arg("-0")
            .arg(pid.to_string())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }
}

/// Web 服务管理状态（嵌入 [`AgentHubRouteHandler`]；进程表 + 启动器 + 探测
/// 时序参数。Clone = Arc 浅克隆，供 spawn_blocking 移交）。
#[derive(Clone)]
pub struct WebState {
    /// 进程表（agentId → 运行条目）。
    procs: Arc<Mutex<std::collections::HashMap<String, WebProc>>>,
    /// 启动器（生产 ProcessWebLauncher；测试注入 mock）。
    launcher: Arc<dyn WebLauncher>,
    /// 端口就绪探测总时限（生产 15s；测试注入毫秒级）。
    probe_timeout: std::time::Duration,
    /// 就绪轮询间隔。
    poll_interval: std::time::Duration,
}

impl WebState {
    /// 生产构造：真实启动器 + 15s 就绪探测。
    #[must_use]
    pub fn new() -> Self {
        Self {
            procs: Arc::new(Mutex::new(std::collections::HashMap::new())),
            launcher: Arc::new(ProcessWebLauncher),
            probe_timeout: WEB_PROBE_TIMEOUT,
            poll_interval: WEB_POLL_INTERVAL,
        }
    }

    /// 测试注入构造：mock 启动器 + 毫秒级探测时序（fake 进程/端口探针，
    /// 绝不真跑 opencode）。
    #[must_use]
    pub fn with_launcher(
        launcher: Arc<dyn WebLauncher>,
        probe_timeout: std::time::Duration,
        poll_interval: std::time::Duration,
    ) -> Self {
        Self {
            procs: Arc::new(Mutex::new(std::collections::HashMap::new())),
            launcher,
            probe_timeout,
            poll_interval,
        }
    }

    /// 追加一行环形日志（条目已不存在则丢弃——停止后读者线程的尾行）。
    fn log_line(&self, agent_id: &str, line: &str) {
        let mut procs = self.procs.lock().expect("web procs poisoned");
        if let Some(p) = procs.get_mut(agent_id) {
            for l in line.lines() {
                if l.trim().is_empty() {
                    continue;
                }
                p.log.push(l.to_string());
                if p.log.len() > WEB_LOG_MAX_LINES {
                    let cut = p.log.len() - WEB_LOG_MAX_LINES;
                    p.log.drain(0..cut);
                }
            }
        }
    }

    /// 表内条目快照。
    fn entry(&self, agent_id: &str) -> Option<WebProc> {
        self.procs
            .lock()
            .expect("web procs poisoned")
            .get(agent_id)
            .cloned()
    }

    /// 死条目清理：表内条目端口已关（进程退出/被外部杀）→ 移除返回 false；
    /// 否则 true。端口探测 500ms 级，caller 包 spawn_blocking。
    fn reconcile_entry(&self, agent_id: &str, port: u16) -> bool {
        let mut procs = self.procs.lock().expect("web procs poisoned");
        let alive = self.launcher.port_open(port);
        if !alive {
            procs.remove(agent_id);
        }
        alive
    }

    /// 重启恢复：表空但端口开（os-api 重启后子进程存活的诚实恢复——pid 与
    /// 启动时间已无从知晓，如实置 None）。返回恢复出的条目。
    fn recover_by_port(&self, agent_id: &str, port: u16) -> WebProc {
        let mut procs = self.procs.lock().expect("web procs poisoned");
        let entry = procs
            .entry(agent_id.to_string())
            .or_insert_with(|| WebProc {
                pid: None,
                port,
                started_at: None,
                log: Vec::new(),
            });
        entry.log.push(
            "[recover] 进程表无此 agent 但端口在监听——按端口探测重建表（os-api 重启后子进程存活；pid/启动时间不可知）".into(),
        );
        entry.clone()
    }

    /// 启动 Web 服务（阻塞：spawn + ≤probe_timeout 端口就绪轮询；caller 包
    /// spawn_blocking）。返回 (结果态, pid)。
    fn start_blocking(
        &self,
        agent_id: &str,
        desc: &AgentWebDesc,
    ) -> Result<(WebStartState, Option<u32>), String> {
        // —— 幂等 / 死条目清理 ——
        if self.entry(agent_id).is_some() && self.reconcile_entry(agent_id, desc.port) {
            let e = self.entry(agent_id).expect("reconcile 后条目仍在");
            return Ok((WebStartState::Idempotent, e.pid));
        }
        // —— 重启恢复：端口已被本 agent 声明端口占用且表无条目 → 重建表返回
        // （不再二次 spawn——opencode serve 端口被占会立即 ServeError 退出）
        if self.launcher.port_open(desc.port) {
            self.recover_by_port(agent_id, desc.port);
            eprintln!(
                "[agenthub] Web 服务 {agent_id} 端口 {} 已被占用且表无条目——按重启恢复处理（pid 未知）",
                desc.port
            );
            return Ok((WebStartState::Recovered, None));
        }
        // —— 真实 spawn ——
        let procs = Arc::clone(&self.procs);
        let sink_agent = agent_id.to_string();
        let on_line: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |line: &str| {
            let mut procs = procs.lock().expect("web procs poisoned");
            if let Some(p) = procs.get_mut(&sink_agent) {
                for l in line.lines() {
                    if l.trim().is_empty() {
                        continue;
                    }
                    p.log.push(l.to_string());
                    if p.log.len() > WEB_LOG_MAX_LINES {
                        let cut = p.log.len() - WEB_LOG_MAX_LINES;
                        p.log.drain(0..cut);
                    }
                }
            }
        });
        let pid = self.launcher.spawn(&desc.start_cmd, Arc::clone(&on_line))?;
        self.procs.lock().expect("web procs poisoned").insert(
            agent_id.to_string(),
            WebProc {
                pid: Some(pid),
                port: desc.port,
                started_at: Some(now_iso()),
                log: Vec::new(),
            },
        );
        // 启动命令行进环形日志（insert 后条目在，log_line 可写；spawn 前调
        // on_line 会被“条目不存在即丢弃”守卫吃掉）
        self.log_line(agent_id, &format!("$ {}", desc.start_cmd.join(" ")));
        // —— 端口就绪轮询（进程早夭 → 立即失败带日志尾）——
        let deadline = std::time::Instant::now() + self.probe_timeout;
        loop {
            if self.launcher.port_open(desc.port) {
                eprintln!("[agenthub] Web 服务 {agent_id} 已就绪（pid {pid}，端口 {}）", desc.port);
                return Ok((WebStartState::Started, Some(pid)));
            }
            if !self.launcher.pid_alive(pid) {
                let tail = self.log_tail(agent_id);
                let _ = self.launcher.kill(pid);
                self.procs.lock().expect("web procs poisoned").remove(agent_id);
                return Err(format!(
                    "进程启动后立即退出（端口 {} 未就绪）{}",
                    desc.port,
                    tail.map(|t| format!("，日志尾：\n{t}")).unwrap_or_default()
                ));
            }
            if std::time::Instant::now() >= deadline {
                let tail = self.log_tail(agent_id);
                let _ = self.launcher.kill(pid);
                self.procs.lock().expect("web procs poisoned").remove(agent_id);
                return Err(format!(
                    "端口 {} 在 {:?} 内未就绪，已终止进程（pid {pid}）{}",
                    desc.port,
                    self.probe_timeout,
                    tail.map(|t| format!("，日志尾：\n{t}")).unwrap_or_default()
                ));
            }
            std::thread::sleep(self.poll_interval);
        }
    }

    /// 停止 Web 服务（阻塞：SIGTERM→SIGKILL；端口仍占 → `fuser -k` 按端口
    /// 兜底；caller 包 spawn_blocking）。未在运行 → Ok(false)；兜底后端口仍
    /// 开 → Err。
    ///
    /// fuser 兜底的实证依据：opencode npm 包的 bin 是 shim，会 spawn 平台
    /// 二进制接管端口（实测 spawn pid 312617 ≠ 持端口 pid 312619）——只杀
    /// 记账 pid 关不掉端口。
    fn stop_blocking(&self, agent_id: &str, port: u16) -> Result<bool, String> {
        if let Some(entry) = self.entry(agent_id) {
            match entry.pid {
                Some(pid) => self.launcher.kill(pid)?,
                None => self.launcher.kill_port(port)?, // 重启恢复条目：pid 不可知
            }
        } else if self.launcher.port_open(port) {
            // 表无条目但端口开（os-api 重启残留 / 手动起的同端口服务）——按端口杀
            self.launcher.kill_port(port)?;
        } else {
            return Ok(false);
        }
        // 端口确认关闭才算停干净（进程退出有竞态，探测前稍等）
        for _ in 0..10 {
            if !self.launcher.port_open(port) {
                self.procs.lock().expect("web procs poisoned").remove(agent_id);
                eprintln!("[agenthub] Web 服务 {agent_id}（端口 {port}）已停止");
                return Ok(true);
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        // 兜底：记账 pid 只是入口 shim 时（opencode 实测），真实服务进程是
        // 其子进程——fuser 按端口杀掉它
        eprintln!(
            "[agenthub] Web 服务 {agent_id} 杀 pid 后端口 {port} 仍开，fuser 按端口兜底"
        );
        self.launcher.kill_port(port)?;
        for _ in 0..10 {
            if !self.launcher.port_open(port) {
                self.procs.lock().expect("web procs poisoned").remove(agent_id);
                eprintln!("[agenthub] Web 服务 {agent_id}（端口 {port}）已停止（按端口兜底）");
                return Ok(true);
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        Err(format!("进程已发终止信号且按端口兜底，端口 {port} 仍被占用（可能被其它进程占用）"))
    }

    /// 状态查询（阻塞：端口探测对账；caller 包 spawn_blocking）。表空但端口
    /// 开 → 重建表返回（running）。
    fn status_blocking(&self, agent_id: &str, port: u16) -> Option<WebProc> {
        if self.entry(agent_id).is_some() {
            if self.reconcile_entry(agent_id, port) {
                return self.entry(agent_id);
            }
            return None; // 死条目已移除
        }
        if self.launcher.port_open(port) {
            return Some(self.recover_by_port(agent_id, port));
        }
        None
    }

    /// 日志尾（最后 20 行拼接；无条目/空日志为 None）。
    fn log_tail(&self, agent_id: &str) -> Option<String> {
        let log = self.entry(agent_id)?.log;
        if log.is_empty() {
            return None;
        }
        let tail: Vec<String> = log.iter().rev().take(20).cloned().collect();
        Some(tail.into_iter().rev().collect::<Vec<_>>().join("\n"))
    }
}

impl Default for WebState {
    fn default() -> Self {
        Self::new()
    }
}

/// `start` 结果态（响应体 `state` 字段：started / idempotent / recovered）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebStartState {
    Started,
    Idempotent,
    Recovered,
}

impl WebStartState {
    fn as_str(self) -> &'static str {
        match self {
            WebStartState::Started => "started",
            WebStartState::Idempotent => "idempotent",
            WebStartState::Recovered => "recovered",
        }
    }
}

/// host:port → host（去端口）。IPv6 `[::1]:8558` → `[::1]`；裸 IPv6/域名/
/// 无数字端口原样返回；`192.168.1.5:8558` → `192.168.1.5`。
fn host_only(hostport: &str) -> String {
    let h = hostport.trim();
    if let Some(rest) = h.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            return format!("[{}]", &rest[..end]);
        }
        return h.to_string();
    }
    match h.rfind(':') {
        Some(i) if !h[i + 1..].is_empty() && h[i + 1..].bytes().all(|b| b.is_ascii_digit()) => {
            h[..i].to_string()
        }
        _ => h.to_string(),
    }
}

/// 从请求头取非空 Host（大小写不敏感）。
fn request_host(req: &ApiRequest) -> Option<String> {
    if let serde_json::Value::Object(map) = &req.headers {
        if let Some((_, v)) = map.iter().find(|(k, _)| k.eq_ignore_ascii_case("host")) {
            if let Some(h) = v.as_str() {
                let h = h.trim();
                if !h.is_empty() {
                    return Some(h.to_string());
                }
            }
        }
    }
    None
}

/// Web 服务 URL 的主机部分推导（三分支，provisioning `source_base_url` 同款
/// 先例）：请求 Host 头（跨机访问即节点 IP/域名——去 API 端口换服务端口）→
/// env `NEXOS_GIT_ADVERTISE_HOST` → `127.0.0.1`。
fn web_base_host(req: &ApiRequest) -> String {
    let advertise = std::env::var(ENV_ADVERTISE_HOST)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    web_base_host_raw(request_host(req).as_deref(), advertise.as_deref())
}

/// [`web_base_host`] 的纯函数核心（可单测不碰 env / 请求）。
fn web_base_host_raw(host: Option<&str>, advertise: Option<&str>) -> String {
    if let Some(h) = host.map(str::trim).filter(|h| !h.is_empty()) {
        return host_only(h);
    }
    if let Some(a) = advertise.map(str::trim).filter(|a| !a.is_empty()) {
        return host_only(a);
    }
    "127.0.0.1".to_string()
}

/// 拼 Web 服务完整 URL：`http://<base_host>:<port><url_path>`。
fn web_url(base_host: &str, desc: &AgentWebDesc) -> String {
    format!("http://{base_host}:{}{}", desc.port, desc.url_path)
}



/// 「Agent 集合」路由处理器——AI coding agent 目录、一键安装/卸载后台任务、
/// 已安装探测、自定义 agent 发布（持久化）与工具链手动安装
/// （子模块 [`agenthub_toolchain`]，node/uv/cargo 用户态安装器）。
pub struct AgentHubRouteHandler {
    /// 自定义 agent（Arc：安装后台任务只动 tasks，发布/删除在请求线程）。
    published: Arc<Mutex<Vec<CatalogAgent>>>,
    /// 自定义 id 序号（与 published 同锁序使用）。
    seq: Arc<Mutex<u64>>,
    /// 任务列表（Arc 克隆进 tokio::spawn 后台等待）。
    tasks: Arc<Mutex<Vec<AgentTask>>>,
    counter: Mutex<u64>,
    /// 持久化文件路径。
    file_path: String,
    /// 工具链手动安装子模块状态（任务表 + 执行器 + 探测根目录）。
    pub(crate) toolchain: agenthub_toolchain::ToolchainState,
    /// Web 界面服务管理子状态（进程表 + 启动器 + 探测时序）。
    pub(crate) web: WebState,
}

impl AgentHubRouteHandler {
    /// 构造 handler（读 env `NEXOS_AGENTHUB_FILE` 指定的持久化文件）。
    #[must_use]
    pub fn new() -> Self {
        let path = std::env::var(ENV_STATE_FILE).unwrap_or_else(|_| DEFAULT_STATE_FILE.into());
        Self::with_state_file(&path)
    }

    /// 指定持久化文件构造（测试注入；缺省路径见 [`DEFAULT_STATE_FILE`]）。
    #[must_use]
    pub fn with_state_file(path: &str) -> Self {
        let st = load_state_from(path);
        Self {
            published: Arc::new(Mutex::new(st.agents)),
            seq: Arc::new(Mutex::new(st.seq)),
            tasks: Arc::new(Mutex::new(Vec::new())),
            counter: Mutex::new(100),
            file_path: path.to_string(),
            toolchain: agenthub_toolchain::ToolchainState::new(),
            web: WebState::new(),
        }
    }

    /// 持久化文件 + 工具链子模块状态构造（测试注入 mock 执行器/固定 HOME 用）。
    #[must_use]
    pub fn with_state_file_and_toolchain(
        path: &str,
        toolchain: agenthub_toolchain::ToolchainState,
    ) -> Self {
        let st = load_state_from(path);
        Self {
            published: Arc::new(Mutex::new(st.agents)),
            seq: Arc::new(Mutex::new(st.seq)),
            tasks: Arc::new(Mutex::new(Vec::new())),
            counter: Mutex::new(100),
            file_path: path.to_string(),
            toolchain,
            web: WebState::new(),
        }
    }

    /// 持久化文件 + Web 子状态构造（测试注入 mock 启动器/毫秒级探测用）。
    #[must_use]
    pub fn with_state_file_and_web(path: &str, web: WebState) -> Self {
        let st = load_state_from(path);
        Self {
            published: Arc::new(Mutex::new(st.agents)),
            seq: Arc::new(Mutex::new(st.seq)),
            tasks: Arc::new(Mutex::new(Vec::new())),
            counter: Mutex::new(100),
            file_path: path.to_string(),
            toolchain: agenthub_toolchain::ToolchainState::new(),
            web,
        }
    }

    /// 全量目录快照（预置 + 自定义，installed 一律 false，由响应端合并探测）。
    fn all_agents_raw(&self) -> Vec<CatalogAgent> {
        let mut out = preset_agents();
        out.extend(
            self.published
                .lock()
                .expect("published poisoned")
                .iter()
                .cloned(),
        );
        out
    }

    /// 目录快照 + installed 探测合并（spawn_blocking）。
    async fn all_agents_detected(&self) -> Result<Vec<CatalogAgent>, ApiGatewayError> {
        let mut agents = self.all_agents_raw();
        let bins: Vec<String> = agents.iter().map(|a| a.check_binary.clone()).collect();
        let found = tokio::task::spawn_blocking(move || detect_binaries_blocking(&bins))
            .await
            .map_err(|e| ApiGatewayError::Internal(format!("已安装探测任务 join 失败: {e}")))?;
        for a in agents.iter_mut() {
            a.installed = found.contains(&a.check_binary);
        }
        Ok(agents)
    }

    /// 当前任务快照。
    #[must_use]
    pub fn tasks_snapshot(&self) -> Vec<AgentTask> {
        self.tasks.lock().expect("tasks poisoned").clone()
    }

    fn next_task_id(&self) -> String {
        let mut c = self.counter.lock().expect("counter poisoned");
        *c += 1;
        format!("task-{}", *c)
    }

    /// 推入任务并裁剪到 [`MAX_TASKS`]。
    fn push_task(&self, task: AgentTask) {
        let mut tasks = self.tasks.lock().expect("tasks poisoned");
        tasks.push(task);
        if tasks.len() > MAX_TASKS {
            let overflow = tasks.len() - MAX_TASKS;
            tasks.drain(0..overflow);
        }
    }

    /// 持久化当前自定义 agent 列表（失败仅记日志，不影响响应）。
    fn persist_published(&self) {
        let st = PersistState {
            agents: self.published.lock().expect("published poisoned").clone(),
            seq: *self.seq.lock().expect("seq poisoned"),
        };
        if let Err(e) = persist_state_to(&self.file_path, &st) {
            eprintln!("[agenthub] 持久化自定义 agent 失败: {e}");
        }
    }
}

impl Default for AgentHubRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for AgentHubRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec(HttpMethod::Get, "/api/v1/agenthub/agents", false, vec![]),
            spec(
                HttpMethod::Get,
                "/api/v1/agenthub/agents/:id",
                false,
                vec![],
            ),
            spec(HttpMethod::Get, "/api/v1/agenthub/installed", false, vec![]),
            spec(
                HttpMethod::Get,
                "/api/v1/agenthub/toolchains",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/agenthub/install",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/agenthub/uninstall",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/agenthub/tasks", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/agenthub/tasks/:id", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/agenthub/publish",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/agenthub/published/:id",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/agenthub/stats", false, vec![]),
            // —— Web 界面服务管理 3 条：start/stop 需 admin，status 公开 ——
            spec(
                HttpMethod::Post,
                "/api/v1/agenthub/web/:agentId/start",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/agenthub/web/:agentId/stop",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/agenthub/web/:agentId/status",
                false,
                vec![],
            ),
        ]
        .into_iter()
        // 工具链手动安装 2 条：POST 需 admin，GET 任务详情公开（契约见
        // agenthub_toolchain::route_specs 与 docs/AGENT_HUB.md）
        .chain(agenthub_toolchain::route_specs())
        .collect()
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        // —— /api/v1/agenthub/toolchain* —— 工具链手动安装（整体委托
        // agenthub_toolchain 子模块：202 异步任务 + 轮询；路由 specs 同源）——
        if matches!(segs.as_slice(), ["api", "v1", "agenthub", "toolchain", ..]) {
            return agenthub_toolchain::handle(&self.toolchain, req.method, &segs[4..], req.body)
                .await;
        }
        let query = query_params(&req.path);
        match (req.method, segs.as_slice()) {
            // —— GET /agents —— 目录（installed 探测合并，支持 ?category= / ?installed=1）
            (HttpMethod::Get, ["api", "v1", "agenthub", "agents"]) => {
                let mut agents = self.all_agents_detected().await?;
                if let Some(cat) = query.get("category") {
                    if !cat.is_empty() {
                        agents.retain(|a| a.category == *cat);
                    }
                }
                if query.get("installed").map(|v| v == "1" || v == "true") == Some(true) {
                    agents.retain(|a| a.installed);
                }
                if query.get("source").map(|v| !v.is_empty()) == Some(true) {
                    if let Some(src) = query.get("source") {
                        agents.retain(|a| a.source == *src);
                    }
                }
                Ok(ok_json(to_value(&agents)?))
            }

            // —— GET /agents/:id —— 详情
            (HttpMethod::Get, ["api", "v1", "agenthub", "agents", id]) => {
                let agents = self.all_agents_detected().await?;
                match agents.iter().find(|a| a.id == *id) {
                    Some(a) => Ok(ok_json(to_value(a)?)),
                    None => Ok(error_response(404, &format!("agent 不存在: {id}"))),
                }
            }

            // —— GET /installed —— 已安装列表（command -v 探测）
            (HttpMethod::Get, ["api", "v1", "agenthub", "installed"]) => {
                let agents = self.all_agents_detected().await?;
                let list: Vec<InstalledAgent> = agents
                    .into_iter()
                    .filter(|a| a.installed)
                    .map(|a| InstalledAgent {
                        id: a.id,
                        name: a.name,
                        binary: a.check_binary,
                    })
                    .collect();
                Ok(ok_json(to_value(&list)?))
            }

            // —— GET /toolchains —— 工具链可用性（node/npm/uv/cargo/curl）
            (HttpMethod::Get, ["api", "v1", "agenthub", "toolchains"]) => {
                let mut out = Vec::new();
                for name in ["node", "npm", "uv", "cargo", "curl"] {
                    let (available, version) = probe_toolchain(name).await;
                    out.push(ToolchainInfo {
                        name: name.into(),
                        available,
                        version,
                    });
                }
                Ok(ok_json(to_value(&out)?))
            }

            // —— POST /install —— 一键安装（admin，后台任务）
            (HttpMethod::Post, ["api", "v1", "agenthub", "install"]) => {
                self.handle_action(req, "install").await
            }

            // —— POST /uninstall —— 卸载（admin；script 渠道 400）
            (HttpMethod::Post, ["api", "v1", "agenthub", "uninstall"]) => {
                self.handle_action(req, "uninstall").await
            }

            // —— GET /tasks —— 任务列表
            (HttpMethod::Get, ["api", "v1", "agenthub", "tasks"]) => {
                Ok(ok_json(to_value(&self.tasks_snapshot())?))
            }

            // —— GET /tasks/:id —— 任务详情
            (HttpMethod::Get, ["api", "v1", "agenthub", "tasks", id]) => {
                match self.tasks_snapshot().into_iter().find(|t| t.id == *id) {
                    Some(t) => Ok(ok_json(to_value(&t)?)),
                    None => Ok(error_response(404, &format!("任务不存在: {id}"))),
                }
            }

            // —— POST /publish —— 发布自定义 agent（admin，持久化）
            (HttpMethod::Post, ["api", "v1", "agenthub", "publish"]) => {
                let body: PublishBody = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析发布请求体失败: {e}")))?;
                let name = body.name.trim();
                let target = body.install_target.trim();
                let binary = body.check_binary.trim();
                let install_type = body.install_type.trim();
                if name.is_empty() {
                    return Ok(error_response(400, "name 不可为空"));
                }
                if !INSTALL_TYPES.contains(&install_type) {
                    return Ok(error_response(
                        400,
                        &format!("install_type 必须是 {:?} 之一", INSTALL_TYPES),
                    ));
                }
                if !is_valid_target(install_type, target) {
                    return Ok(error_response(
                        400,
                        "install_target 不合法（script 须为 http(s):// URL，其余须为无空白的包名）",
                    ));
                }
                if !is_safe_binary_name(binary) {
                    return Ok(error_response(
                        400,
                        "check_binary 不合法（仅字母数字与 . _ -，≤64 字符）",
                    ));
                }
                // 预置目录同 binary 提示但不拒绝（用户可能装自己的 fork）
                let homepage = body.homepage.trim();
                if !homepage.is_empty() && !homepage.starts_with("http") {
                    return Ok(error_response(400, "homepage 须为 http(s):// URL"));
                }
                let seq = {
                    let mut s = self.seq.lock().expect("seq poisoned");
                    *s += 1;
                    *s
                };
                let agent = CatalogAgent {
                    id: format!("custom-{seq}"),
                    name: name.to_string(),
                    description: body.description.trim().to_string(),
                    category: if body.category.trim().is_empty() {
                        "custom".into()
                    } else {
                        body.category.trim().to_string()
                    },
                    icon: "🧩".into(),
                    source: "user".into(),
                    install_type: install_type.to_string(),
                    install_target: target.to_string(),
                    check_binary: binary.to_string(),
                    homepage: homepage.to_string(),
                    publisher: "用户发布".into(),
                    tags: body
                        .tags
                        .iter()
                        .map(|t| t.trim().to_string())
                        .filter(|t| !t.is_empty())
                        .collect(),
                    installed: false,
                    web: None, // 用户发布不收 web 描述符（仅预置目录实测标注）
                };
                let resp_body = to_value(&agent)?;
                self.published
                    .lock()
                    .expect("published poisoned")
                    .push(agent);
                self.persist_published();
                Ok(ApiResponse {
                    status: 201,
                    body: resp_body,
                    headers: serde_json::json!({}),
                })
            }

            // —— DELETE /published/:id —— 删自定义 agent（admin，仅 source=user 可删）
            (HttpMethod::Delete, ["api", "v1", "agenthub", "published", id]) => {
                {
                    let mut published = self.published.lock().expect("published poisoned");
                    let before = published.len();
                    published.retain(|a| !(a.id == *id && a.source == "user"));
                    if published.len() == before {
                        return Ok(error_response(
                            404,
                            &format!("自定义 agent 不存在（预置 agent 不可删）: {id}"),
                        ));
                    }
                }
                self.persist_published();
                Ok(ok_json(serde_json::json!({
                    "ok": true, "id": id, "action": "delete"
                })))
            }

            // —— GET /stats —— 聚合统计
            (HttpMethod::Get, ["api", "v1", "agenthub", "stats"]) => {
                let agents = self.all_agents_detected().await?;
                let installed = agents.iter().filter(|a| a.installed).count();
                let mut toolchains_ready = 0;
                for name in ["node", "npm", "uv", "cargo", "curl"] {
                    let (available, _) = probe_toolchain(name).await;
                    if available {
                        toolchains_ready += 1;
                    }
                }
                Ok(ok_json(to_value(&AgentHubStats {
                    total_agents: agents.len(),
                    installed,
                    toolchains_ready,
                    tasks: self.tasks_snapshot().len(),
                })?))
            }

            // —— Web 界面服务管理（start/stop/status；仅 web 描述符标注的 agent）——
            (HttpMethod::Post, ["api", "v1", "agenthub", "web", id, "start"]) => {
                let agent_id = id.to_string();
                self.handle_web_start(req, &agent_id).await
            }
            (HttpMethod::Post, ["api", "v1", "agenthub", "web", id, "stop"]) => {
                let agent_id = id.to_string();
                self.handle_web_stop(&agent_id).await
            }
            (HttpMethod::Get, ["api", "v1", "agenthub", "web", id, "status"]) => {
                let agent_id = id.to_string();
                self.handle_web_status(req, &agent_id).await
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "agenthub: 未匹配的路由")),
        }
    }
}

impl AgentHubRouteHandler {
    /// install / uninstall 共用动作处理。
    async fn handle_action(
        &self,
        req: ApiRequest,
        action: &'static str,
    ) -> Result<ApiResponse, ApiGatewayError> {
        let body: ActionBody = serde_json::from_value(req.body)
            .map_err(|e| ApiGatewayError::Internal(format!("解析{action}请求体失败: {e}")))?;
        if body.agent_id.trim().is_empty() {
            return Ok(error_response(400, "agent_id 不可为空"));
        }
        let agents = self.all_agents_raw();
        let agent = match agents.iter().find(|a| a.id == body.agent_id) {
            Some(a) => a.clone(),
            None => {
                return Ok(error_response(
                    404,
                    &format!("agent 不存在: {}", body.agent_id),
                ))
            }
        };
        // script 渠道无卸载命令：明确 400（不建任务）
        if action == "uninstall" && agent.install_type == "script" {
            return Ok(error_response(
                400,
                &format!(
                    "{} 为官方脚本安装（script 渠道），不支持一键卸载",
                    agent.name
                ),
            ));
        }
        let task = self.spawn_action(&agent, action).await?;
        Ok(ApiResponse {
            status: 201,
            body: to_value(&task)?,
            headers: serde_json::json!({}),
        })
    }

    /// 创建并 spawn 安装/卸载后台任务（fire-and-forget：请求立即返回，
    /// 进程退出后经共享 `tasks` 回写 status/error/log_tail）。
    ///
    /// 命令为空（未知渠道）→ 任务直接 failed 不 spawn。npm 渠道按策略 env /
    /// 前缀写探测决定是否前置 sudo（stdin 已 null，sudo 需密码时立即失败不挂起）。
    async fn spawn_action(
        &self,
        agent: &CatalogAgent,
        action: &str,
    ) -> Result<AgentTask, ApiGatewayError> {
        let mut task = AgentTask {
            id: self.next_task_id(),
            agent_id: agent.id.clone(),
            agent_name: agent.name.clone(),
            action: action.into(),
            install_type: agent.install_type.clone(),
            status: "pending".into(),
            pid: None,
            error: None,
            log_tail: None,
            created_at: now_iso(),
        };

        let npm_sudo = if agent.install_type == "npm" {
            match npm_sudo_policy() {
                Some(v) => v,
                None => tokio::task::spawn_blocking(npm_needs_sudo_blocking)
                    .await
                    .map_err(|e| {
                        ApiGatewayError::Internal(format!("npm sudo 探测 join 失败: {e}"))
                    })?,
            }
        } else {
            false
        };

        let mut cmd = if action == "install" {
            build_install_cmd(&agent.install_type, &agent.install_target, npm_sudo)
        } else {
            build_uninstall_cmd(&agent.install_type, &agent.install_target, npm_sudo)
        };

        if cmd.is_empty() {
            task.status = "failed".into();
            task.error = Some(format!(
                "不支持的{action}渠道：{}（无对应命令）",
                agent.install_type
            ));
            self.push_task(task.clone());
            return Ok(task);
        }

        // 程序名显式解析：npm/uv/cargo 可能装在用户级 bin（systemd 服务 PATH
        // 不含 ~/.local/bin、~/.cargo/bin），直接按名字 spawn 会 ENOENT。
        // sudo 包裹时解析其后的工具名（sudo 自身用 PATH 即可）；bash -c 的
        // curl|bash 内联体不展开（bash/curl 均为系统路径）。
        if cmd.first().is_some_and(|p| p == "sudo") {
            if let Some(tool) = cmd.get(1) {
                let resolved = resolve_bin(tool);
                cmd[1] = resolved;
            }
        } else if cmd.first().is_some_and(|p| p != "bash") {
            if let Some(program) = cmd.first() {
                let resolved = resolve_bin(program);
                cmd[0] = resolved;
            }
        }

        let program = cmd[0].clone();
        let args: Vec<String> = cmd[1..].to_vec();
        let mut proc = tokio::process::Command::new(&program);
        proc.args(&args);
        proc.stdin(std::process::Stdio::null());
        proc.stdout(std::process::Stdio::piped());
        proc.stderr(std::process::Stdio::piped());

        match proc.spawn() {
            Ok(child) => {
                task.pid = child.id();
                task.status = "running".into();
                self.push_task(task.clone());
                let tasks = Arc::clone(&self.tasks);
                let task_id = task.id.clone();
                tokio::spawn(async move {
                    let result = child.wait_with_output().await;
                    let (status, error, log_tail) = match result {
                        Ok(out) if out.status.success() => ("completed".to_string(), None, None),
                        Ok(out) => {
                            let stderr = String::from_utf8_lossy(&out.stderr);
                            let stdout = String::from_utf8_lossy(&out.stdout);
                            let combined = if !stderr.is_empty() { stderr } else { stdout };
                            let tail = combined
                                .lines()
                                .rev()
                                .take(10)
                                .collect::<Vec<_>>()
                                .into_iter()
                                .rev()
                                .collect::<Vec<_>>()
                                .join("\n");
                            (
                                "failed".to_string(),
                                Some(format!("退出码 {:?}", out.status.code())),
                                Some(tail),
                            )
                        }
                        Err(e) => (
                            "failed".to_string(),
                            Some(format!("进程等待失败: {e}")),
                            None,
                        ),
                    };
                    let mut tasks = tasks.lock().expect("tasks poisoned");
                    if let Some(t) = tasks.iter_mut().find(|t| t.id == task_id) {
                        t.status = status;
                        t.pid = None;
                        t.error = error;
                        t.log_tail = log_tail;
                    }
                });
                Ok(task)
            }
                Err(e) => {
                task.status = "failed".into();
                task.error = Some(format!("命令启动失败（{program} 可能未安装）: {e}"));
                self.push_task(task.clone());
                Ok(task)
            }
        }
    }

    // —— Web 界面端点（start / stop / status；仅 web 描述符标注的 agent）——

    /// `POST /web/:agentId/start`（admin）：spawn `start_cmd`（阻塞部分搬
    /// spawn_blocking）→ 端口就绪（≤15s）→ `200 {url, pid, port, state}`；
    /// 已在跑幂等返回；端口被占且表丢失 → 恢复态返回；失败 `500 {error}`
    /// （错误串已带日志尾）。
    async fn handle_web_start(
        &self,
        req: ApiRequest,
        id: &str,
    ) -> Result<ApiResponse, ApiGatewayError> {
        let Some(agent) = self.all_agents_raw().into_iter().find(|a| a.id == id) else {
            return Ok(error_response(404, &format!("agent 不存在: {id}")));
        };
        let Some(desc) = agent.web.clone() else {
            return Ok(error_response(
                400,
                &format!("{} 未标注 Web 界面描述符（仅实测确认有 Web UI 的 agent 提供）", agent.name),
            ));
        };
        let base_host = web_base_host(&req);
        let web = self.web.clone();
        let agent_id = id.to_string();
        let desc_for_job = desc.clone();
        let started = tokio::task::spawn_blocking(move || {
            web.start_blocking(&agent_id, &desc_for_job)
        })
        .await
        .map_err(|e| ApiGatewayError::Internal(format!("Web 启动任务 join 失败: {e}")))?;
        match started {
            Ok((state, pid)) => Ok(ok_json(serde_json::json!({
                "agent_id": id,
                "url": web_url(&base_host, &desc),
                "pid": pid,
                "port": desc.port,
                "state": state.as_str(),
            }))),
            Err(e) => {
                eprintln!("[agenthub] Web 服务 {id} 启动失败：{e}");
                Ok(error_response(500, &e))
            }
        }
    }

    /// `POST /web/:agentId/stop`（admin）：终止服务进程（恢复条目按端口杀）→
    /// `200 {ok:true}`；未在运行 404；停完端口仍占 500。
    async fn handle_web_stop(&self, id: &str) -> Result<ApiResponse, ApiGatewayError> {
        let Some(agent) = self.all_agents_raw().into_iter().find(|a| a.id == id) else {
            return Ok(error_response(404, &format!("agent 不存在: {id}")));
        };
        let Some(desc) = agent.web.clone() else {
            return Ok(error_response(
                400,
                &format!("{} 未标注 Web 界面描述符", agent.name),
            ));
        };
        let web = self.web.clone();
        let agent_id = id.to_string();
        let stopped = tokio::task::spawn_blocking(move || web.stop_blocking(&agent_id, desc.port))
            .await
            .map_err(|e| ApiGatewayError::Internal(format!("Web 停止任务 join 失败: {e}")))?;
        match stopped {
            Ok(true) => Ok(ok_json(
                serde_json::json!({"ok": true, "agent_id": id, "action": "web_stop"}),
            )),
            Ok(false) => Ok(error_response(404, "Web 服务未在运行")),
            Err(e) => {
                eprintln!("[agenthub] Web 服务 {id} 停止失败：{e}");
                Ok(error_response(500, &e))
            }
        }
    }

    /// `GET /web/:agentId/status`（公开）：`{running, url, pid, port,
    /// started_at, log_tail}`；表空但端口开 → 重建表（os-api 重启恢复）。
    async fn handle_web_status(
        &self,
        req: ApiRequest,
        id: &str,
    ) -> Result<ApiResponse, ApiGatewayError> {
        let Some(agent) = self.all_agents_raw().into_iter().find(|a| a.id == id) else {
            return Ok(error_response(404, &format!("agent 不存在: {id}")));
        };
        let Some(desc) = agent.web.clone() else {
            return Ok(error_response(
                400,
                &format!("{} 未标注 Web 界面描述符", agent.name),
            ));
        };
        let web = self.web.clone();
        let agent_id = id.to_string();
        let port = desc.port;
        let entry = tokio::task::spawn_blocking(move || web.status_blocking(&agent_id, port))
            .await
            .map_err(|e| ApiGatewayError::Internal(format!("Web 状态任务 join 失败: {e}")))?;
        match entry {
            Some(e) => Ok(ok_json(serde_json::json!({
                "agent_id": id,
                "running": true,
                "url": web_url(&web_base_host(&req), &desc),
                "pid": e.pid,
                "port": e.port,
                "started_at": e.started_at,
                "log_tail": self.web.log_tail(id),
            }))),
            None => Ok(ok_json(serde_json::json!({
                "agent_id": id,
                "running": false,
                "url": null,
                "pid": null,
                "port": desc.port,
                "started_at": null,
                "log_tail": null,
            }))),
        }
    }
}

/// 探测单个工具链（--version，3s 超时；失败返回不可用）。
/// 程序名先经 [`resolve_bin`] 解析（uv/cargo 可能仅在用户级 bin）。
async fn probe_toolchain(name: &str) -> (bool, String) {
    let program = resolve_bin(name);
    let res = tokio::time::timeout(TOOLCHAIN_TIMEOUT, async move {
        let out = tokio::process::Command::new(&program)
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .await?;
        if out.status.success() {
            let first = String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            Ok::<String, std::io::Error>(first)
        } else {
            Err(std::io::Error::other("exit non-zero"))
        }
    })
    .await;
    match res {
        Ok(Ok(version)) => (true, version),
        _ => (false, String::new()),
    }
}

// ----------------------------------------------------------------------------
// 内部辅助（app_store.rs 同款，不共享避免跨文件耦合）
// ----------------------------------------------------------------------------

fn spec(
    method: HttpMethod,
    path: &str,
    requires_auth: bool,
    required_roles: Vec<String>,
) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: COMPONENT.to_string(),
        requires_auth,
        required_roles,
    }
}

fn ok_json(body: serde_json::Value) -> ApiResponse {
    ApiResponse {
        status: 200,
        body,
        headers: serde_json::json!({}),
    }
}

fn error_response(status: u16, msg: &str) -> ApiResponse {
    ApiResponse {
        status,
        body: serde_json::json!({"error": msg}),
        headers: serde_json::json!({}),
    }
}

fn to_value<T: Serialize>(v: &T) -> Result<serde_json::Value, ApiGatewayError> {
    serde_json::to_value(v).map_err(|e| ApiGatewayError::Internal(format!("响应序列化失败: {e}")))
}

fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

/// 解析 query string（仅 key=value，重复取最后一个）。
fn query_params(path: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    if let Some(q) = path.split('?').nth(1) {
        for pair in q.split('&') {
            let mut it = pair.splitn(2, '=');
            if let Some(k) = it.next() {
                if k.is_empty() {
                    continue;
                }
                let v = it.next().unwrap_or("");
                out.insert(k.to_string(), v.to_string());
            }
        }
    }
    out
}

fn now_iso() -> String {
    use chrono::Local;
    Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn get_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    fn post_req(path: &str, body: serde_json::Value) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Post,
            path: path.into(),
            headers: serde_json::json!({}),
            body,
            auth: None,
        }
    }

    fn del_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Delete,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    /// 临时持久化目录（workspace 无 tempfile，自管清理）。
    struct TempDirGuard {
        dir: std::path::PathBuf,
    }
    impl TempDirGuard {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "agenthub-test-{tag}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0)
            ));
            std::fs::create_dir_all(&dir).expect("创建临时目录");
            Self { dir }
        }
        fn path(&self, name: &str) -> String {
            self.dir.join(name).to_string_lossy().into_owned()
        }
    }
    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    // ---- 路由声明 ----

    #[tokio::test]
    async fn routes_declares_sixteen_endpoints_all_agenthub() {
        let h = AgentHubRouteHandler::new();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 16, "应有 16 条路由: {routes:?}");
        assert!(
            routes.iter().all(|r| r.handler_component == "agenthub"),
            "全部归属 agenthub 组件"
        );
        for r in &routes {
            if r.method == HttpMethod::Post || r.method == HttpMethod::Delete {
                assert!(r.requires_auth, "写操作需 auth: {r:?}");
                assert_eq!(r.required_roles, vec!["admin".to_string()]);
            } else {
                assert!(!r.requires_auth, "GET 应公开: {r:?}");
            }
        }
        // 工具链安装 2 条新路由在列
        assert!(
            routes
                .iter()
                .any(|r| r.path == "/api/v1/agenthub/toolchain/install"),
            "缺工具链安装 POST 路由"
        );
        assert!(
            routes
                .iter()
                .any(|r| r.path == "/api/v1/agenthub/toolchain/install/tasks/:id"),
            "缺工具链任务 GET 路由"
        );
        // Web 界面 3 条路由在列（start/stop admin，status 公开）
        assert!(
            routes
                .iter()
                .any(|r| r.path == "/api/v1/agenthub/web/:agentId/start" && r.requires_auth),
            "缺 Web start 路由"
        );
        assert!(
            routes
                .iter()
                .any(|r| r.path == "/api/v1/agenthub/web/:agentId/stop" && r.requires_auth),
            "缺 Web stop 路由"
        );
        assert!(
            routes.iter().any(|r| r.path == "/api/v1/agenthub/web/:agentId/status"
                && !r.requires_auth),
            "缺 Web status 路由"
        );
    }

    // ---- 预置目录 ----

    #[test]
    fn preset_agents_contains_common_ai_agents() {
        let agents = preset_agents();
        assert!(agents.len() >= 8, "预置应 >=8 条: {}", agents.len());
        for want in [
            "opencode",
            "openclaw",
            "claude-code",
            "codex",
            "gemini-cli",
            "aider",
        ] {
            assert!(agents.iter().any(|a| a.id == want), "应含 {want}");
        }
        for a in &agents {
            assert!(!a.name.is_empty() && !a.description.is_empty());
            assert!(
                INSTALL_TYPES.contains(&a.install_type.as_str()),
                "{} 渠道非法",
                a.id
            );
            assert!(
                is_valid_target(&a.install_type, &a.install_target),
                "{} 目标非法",
                a.id
            );
            assert!(
                is_safe_binary_name(&a.check_binary),
                "{} 二进制名非法",
                a.id
            );
            assert_eq!(a.source, "preset");
        }
        // check_binary 不得重复（探测按 binary 判定，重复会互相污染）
        let mut bins: Vec<&str> = agents.iter().map(|a| a.check_binary.as_str()).collect();
        bins.sort_unstable();
        bins.dedup();
        assert_eq!(bins.len(), agents.len(), "check_binary 应唯一");
        // web 描述符：仅 OpenCode 标注（实测确认 Web UI），其余一律 None——
        // 未实测的不猜，前端不显示「打开界面」按钮
        let with_web: Vec<&str> = agents
            .iter()
            .filter(|a| a.web.is_some())
            .map(|a| a.id.as_str())
            .collect();
        assert_eq!(with_web, vec!["opencode"], "仅 OpenCode 应标注 web: {with_web:?}");
        let oc = agents.iter().find(|a| a.id == "opencode").unwrap();
        let web = oc.web.as_ref().unwrap();
        assert!(!web.start_cmd.is_empty(), "start_cmd 非空");
        assert!(
            web.start_cmd.contains(&"--port".to_string())
                && web.start_cmd.contains(&web.port.to_string()),
            "start_cmd 应显式固定端口（--port 缺省 0=随机）: {:?}",
            web.start_cmd
        );
        assert!(web.url_path.starts_with('/'), "url_path 须含前导 /");
        assert!(!web.note.is_empty(), "note 应记录实测鉴权形态");
    }

    // ---- 命令构造器 ----

    #[test]
    fn build_install_cmd_shapes() {
        // npm 无 sudo
        assert_eq!(
            build_install_cmd("npm", "opencode-ai", false),
            vec!["npm", "install", "-g", "opencode-ai"]
        );
        // npm 带 sudo（系统级前缀不可写）
        assert_eq!(
            build_install_cmd("npm", "@anthropic-ai/claude-code", true),
            vec!["sudo", "npm", "install", "-g", "@anthropic-ai/claude-code"]
        );
        // script：curl | bash
        let script = build_install_cmd("script", "https://x.example/install.sh", false);
        assert_eq!(script[0], "bash");
        assert_eq!(script[1], "-c");
        assert!(script[2].contains("curl -fsSL https://x.example/install.sh | bash"));
        // uv / cargo
        assert_eq!(
            build_install_cmd("uv", "aider-chat", false),
            vec!["uv", "tool", "install", "aider-chat"]
        );
        assert_eq!(
            build_install_cmd("cargo", "ripgrep", false),
            vec!["cargo", "install", "ripgrep"]
        );
        // 未知渠道：空（不 spawn）
        assert!(build_install_cmd("apt", "x", false).is_empty());
        assert!(build_install_cmd("", "x", false).is_empty());
    }

    #[test]
    fn build_uninstall_cmd_shapes() {
        assert_eq!(
            build_uninstall_cmd("npm", "opencode-ai", false),
            vec!["npm", "uninstall", "-g", "opencode-ai"]
        );
        assert_eq!(
            build_uninstall_cmd("npm", "opencode-ai", true),
            vec!["sudo", "npm", "uninstall", "-g", "opencode-ai"]
        );
        assert_eq!(
            build_uninstall_cmd("uv", "aider-chat", false),
            vec!["uv", "tool", "uninstall", "aider-chat"]
        );
        assert_eq!(
            build_uninstall_cmd("cargo", "ripgrep", false),
            vec!["cargo", "uninstall", "ripgrep"]
        );
        // script / 未知：空（HTTP 层 400）
        assert!(build_uninstall_cmd("script", "https://x", false).is_empty());
        assert!(build_uninstall_cmd("nope", "x", false).is_empty());
    }

    // ---- 校验纯函数 ----

    #[test]
    fn binary_name_whitelist_blocks_injection() {
        assert!(is_safe_binary_name("opencode"));
        assert!(is_safe_binary_name("aider-chat"));
        assert!(!is_safe_binary_name("a; rm -rf /"));
        assert!(!is_safe_binary_name("x$(id)"));
        assert!(!is_safe_binary_name(""));
        assert!(!is_safe_binary_name("带空格 name"));
    }

    #[test]
    fn resolve_bin_prefers_user_local_dirs() {
        let guard = TempDirGuard::new("resolve");
        let home = guard.dir.to_string_lossy().into_owned();
        let bin_dir = std::path::Path::new(&home).join(".local").join("bin");
        std::fs::create_dir_all(&bin_dir).expect("创建 .local/bin");
        std::fs::write(bin_dir.join("uv"), "#!/bin/sh\n").expect("写 uv 占位");
        // 用户级存在 → 绝对路径
        let r = resolve_bin_in(&home, "uv");
        assert!(
            r.starts_with(&home) && r.ends_with(".local/bin/uv"),
            "应解析为用户级绝对路径: {r}"
        );
        // 不存在 → 原名返回（交还 PATH 兜底）
        assert_eq!(resolve_bin_in(&home, "cargo"), "cargo");
        // 危险名 / 空 HOME → 原样返回不拼接
        assert_eq!(resolve_bin_in(&home, "a;b"), "a;b");
        assert_eq!(resolve_bin_in("", "uv"), "uv");
    }

    #[test]
    fn resolve_bin_falls_back_to_nvm_version_dirs_picking_highest() {
        let guard = TempDirGuard::new("resolve-nvm");
        let home = guard.dir.to_string_lossy().into_owned();
        // 两个 nvm 版本目录（字典序 v9.11 > v10.0 是错排陷阱，数值比较应取 v10）
        for (ver, with) in [("v9.11.0", true), ("v10.0.0", true), ("v8.1.0", false)] {
            let bin = std::path::Path::new(&home)
                .join(".nvm")
                .join("versions")
                .join("node")
                .join(ver)
                .join("bin");
            std::fs::create_dir_all(&bin).expect("创建 nvm 版本 bin");
            if with {
                std::fs::write(bin.join("node"), "#!/bin/sh\n").expect("写 node 占位");
            }
        }
        let r = resolve_bin_in(&home, "node");
        assert!(
            r.ends_with(".nvm/versions/node/v10.0.0/bin/node"),
            "应取最高版本 v10.0.0: {r}"
        );
        // 版本目录无该文件 → 原名返回
        assert_eq!(resolve_bin_in(&home, "npm"), "npm");
        // nvm_bin_dirs 排序本身：v10 > v9 > v8（数值降序；v8 无 node 但 bin 目录在）
        let dirs = nvm_bin_dirs(&home);
        let names: Vec<String> = dirs
            .iter()
            .map(|d| d.parent().unwrap().file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["v10.0.0".to_string(), "v9.11.0".to_string(), "v8.1.0".to_string()],
            "版本目录数值降序: {names:?}"
        );
    }

    #[test]
    fn detect_binaries_in_hits_nvm_dir_without_path() {
        let guard = TempDirGuard::new("detect-nvm");
        let home = guard.dir.to_string_lossy().into_owned();
        // nvm 目录放一个宿主 PATH 上肯定没有的二进制名，隔离 command -v 干扰
        let bin = std::path::Path::new(&home)
            .join(".nvm")
            .join("versions")
            .join("node")
            .join("v22.11.0")
            .join("bin");
        std::fs::create_dir_all(&bin).expect("创建 nvm bin");
        std::fs::write(bin.join("nexos-nvm-only-bin"), "#!/bin/sh\n").expect("写占位");
        let found = detect_binaries_in(&home, &["nexos-nvm-only-bin".to_string()]);
        assert!(
            found.contains("nexos-nvm-only-bin"),
            "nvm 版本目录应兜底命中: {found:?}"
        );
        let miss = detect_binaries_in(&home, &["nexos-no-such-bin".to_string()]);
        assert!(miss.is_empty(), "不存在的不应命中: {miss:?}");
    }

    #[test]
    fn target_validation_by_channel() {
        assert!(is_valid_target("npm", "@anthropic-ai/claude-code"));
        assert!(!is_valid_target("npm", "has space"));
        assert!(is_valid_target(
            "script",
            "https://block.github.io/goose/x.sh"
        ));
        assert!(!is_valid_target("script", "ftp://x"));
        assert!(!is_valid_target("script", "javascript:alert(1)"));
        assert!(!is_valid_target("npm", ""));
    }

    // ---- GET /agents ----

    #[tokio::test]
    async fn list_agents_returns_catalog_with_installed_flag() {
        let h = AgentHubRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/agenthub/agents")).await.unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().expect("body 为数组");
        assert!(arr.len() >= 8);
        for a in arr {
            assert!(a["installed"].is_boolean(), "应含 installed 布尔: {a:?}");
        }
        assert!(arr.iter().any(|a| a["id"] == "opencode"));
    }

    #[tokio::test]
    async fn list_agents_category_filter() {
        let h = AgentHubRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/agenthub/agents?category=assistant"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().unwrap();
        assert!(!arr.is_empty());
        assert!(arr.iter().all(|a| a["category"] == "assistant"));
    }

    #[tokio::test]
    async fn get_agent_detail_and_missing() {
        let h = AgentHubRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/agenthub/agents/openclaw"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["id"], "openclaw");
        assert_eq!(resp.body["install_type"], "npm");

        let resp = h
            .handle(get_req("/api/v1/agenthub/agents/__nope__"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // ---- 探测端点（真实系统，不 panic 即可）----

    #[tokio::test]
    async fn installed_and_toolchains_return_without_panic() {
        let h = AgentHubRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/agenthub/installed"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body.is_array());
        for e in resp.body.as_array().unwrap() {
            assert!(e["binary"].is_string());
        }

        let resp = h
            .handle(get_req("/api/v1/agenthub/toolchains"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 5, "node/npm/uv/cargo/curl 五项");
        for t in arr {
            assert!(t["available"].is_boolean());
        }
    }

    // ---- install / uninstall 边界 ----

    #[tokio::test]
    async fn install_missing_agent_returns_404() {
        let h = AgentHubRouteHandler::new();
        let resp = h
            .handle(post_req(
                "/api/v1/agenthub/install",
                serde_json::json!({"agent_id": "__nope__"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn install_rejects_empty_agent_id() {
        let h = AgentHubRouteHandler::new();
        let resp = h
            .handle(post_req(
                "/api/v1/agenthub/install",
                serde_json::json!({"agent_id": " "}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400);
    }

    #[tokio::test]
    async fn uninstall_script_agent_rejected_400() {
        let h = AgentHubRouteHandler::new();
        let resp = h
            .handle(post_req(
                "/api/v1/agenthub/uninstall",
                serde_json::json!({"agent_id": "goose"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "script 渠道应拒卸载: {resp:?}");
        // 不产生任务
        let resp = h.handle(get_req("/api/v1/agenthub/tasks")).await.unwrap();
        assert_eq!(resp.body.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn install_unsupported_channel_fails_task_without_spawn() {
        // 经私有字段注入渠道非法的条目（publish 校验已拦正常路径），验证任务
        // 直接 failed 且不 spawn 外部进程
        let h = AgentHubRouteHandler::new();
        h.published.lock().unwrap().push(CatalogAgent {
            id: "custom-test".into(),
            name: "测试".into(),
            description: String::new(),
            category: "custom".into(),
            icon: "🧩".into(),
            source: "user".into(),
            install_type: "bogus".into(),
            install_target: "x".into(),
            check_binary: "x".into(),
            homepage: String::new(),
            publisher: "测试".into(),
            tags: vec![],
            installed: false,
            web: None,
        });
        let resp = h
            .handle(post_req(
                "/api/v1/agenthub/install",
                serde_json::json!({"agent_id": "custom-test"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["status"], "failed", "未知渠道任务应 failed");
        assert!(resp.body["error"].as_str().unwrap().contains("不支持"));
    }

    // ---- 任务端点 ----

    #[tokio::test]
    async fn task_detail_and_missing() {
        let h = AgentHubRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/agenthub/tasks/task-999"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    #[tokio::test]
    async fn push_task_trims_to_cap() {
        let h = AgentHubRouteHandler::new();
        for i in 0..MAX_TASKS + 20 {
            h.push_task(AgentTask {
                id: format!("t{i}"),
                agent_id: "x".into(),
                agent_name: "x".into(),
                action: "install".into(),
                install_type: "npm".into(),
                status: "completed".into(),
                pid: None,
                error: None,
                log_tail: None,
                created_at: String::new(),
            });
        }
        assert_eq!(h.tasks_snapshot().len(), MAX_TASKS);
        assert_eq!(h.tasks_snapshot()[0].id, "t20", "应裁最旧");
    }

    // ---- publish 持久化 ----

    #[tokio::test]
    async fn publish_persists_and_survives_reopen() {
        let guard = TempDirGuard::new("persist");
        let path = guard.path("agents.json");
        let h = AgentHubRouteHandler::with_state_file(&path);
        let resp = h
            .handle(post_req(
                "/api/v1/agenthub/publish",
                serde_json::json!({
                    "name": "MyAgent",
                    "description": "自定义",
                    "install_type": "npm",
                    "install_target": "my-agent-cli",
                    "check_binary": "myagent",
                    "homepage": "https://example.com"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "publish: {resp:?}");
        assert_eq!(resp.body["source"], "user");
        let id = resp.body["id"].as_str().unwrap().to_string();
        assert!(id.starts_with("custom-"));

        // 文件已落盘
        assert!(std::path::Path::new(&path).exists());

        // 重开 handler（模拟重启）仍在目录里，且 id 序号不回退（下一个 custom-2）
        let h2 = AgentHubRouteHandler::with_state_file(&path);
        let resp = h2.handle(get_req("/api/v1/agenthub/agents")).await.unwrap();
        let arr = resp.body.as_array().unwrap();
        assert!(arr.iter().any(|a| a["id"] == id), "重启后自定义仍在");
        let resp = h2
            .handle(post_req(
                "/api/v1/agenthub/publish",
                serde_json::json!({
                    "name": "Again", "install_type": "npm",
                    "install_target": "a2", "check_binary": "a2"
                }),
            ))
            .await
            .unwrap();
        assert_ne!(resp.body["id"], serde_json::json!(id), "重启后 id 不复用");
    }

    #[tokio::test]
    async fn publish_rejects_bad_bodies() {
        let guard = TempDirGuard::new("reject");
        let h = AgentHubRouteHandler::with_state_file(&guard.path("a.json"));
        let cases = [
            serde_json::json!({"name": "", "install_type": "npm", "install_target": "x", "check_binary": "x"}),
            serde_json::json!({"name": "ok", "install_type": "apt", "install_target": "x", "check_binary": "x"}),
            serde_json::json!({"name": "ok", "install_type": "npm", "install_target": "", "check_binary": "x"}),
            serde_json::json!({"name": "ok", "install_type": "npm", "install_target": "has space", "check_binary": "x"}),
            serde_json::json!({"name": "ok", "install_type": "script", "install_target": "not-url", "check_binary": "x"}),
            serde_json::json!({"name": "ok", "install_type": "npm", "install_target": "x", "check_binary": "bad;name"}),
            serde_json::json!({"name": "ok", "install_type": "npm", "install_target": "x", "check_binary": "x", "homepage": "ftp://x"}),
        ];
        for (i, body) in cases.iter().enumerate() {
            let resp = h
                .handle(post_req("/api/v1/agenthub/publish", body.clone()))
                .await
                .unwrap();
            assert_eq!(resp.status, 400, "case #{i} 应 400: {resp:?}");
        }
    }

    #[tokio::test]
    async fn delete_published_roundtrip() {
        let guard = TempDirGuard::new("delete");
        let h = AgentHubRouteHandler::with_state_file(&guard.path("a.json"));
        let resp = h
            .handle(post_req(
                "/api/v1/agenthub/publish",
                serde_json::json!({
                    "name": "待删", "install_type": "npm",
                    "install_target": "x", "check_binary": "x"
                }),
            ))
            .await
            .unwrap();
        let id = resp.body["id"].as_str().unwrap().to_string();

        // 预置 agent 不可删
        let resp = h
            .handle(del_req("/api/v1/agenthub/published/opencode"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404, "预置不可删");

        let resp = h
            .handle(del_req(&format!("/api/v1/agenthub/published/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);

        // 删后再开：持久化文件里也没有了
        let h2 = AgentHubRouteHandler::with_state_file(&guard.path("a.json"));
        let resp = h2
            .handle(get_req("/api/v1/agenthub/agents?source=user"))
            .await
            .unwrap();
        let arr = resp.body.as_array().unwrap();
        assert!(!arr.iter().any(|a| a["id"] == id), "删除应持久化");

        // 删不存在 → 404
        let resp = h
            .handle(del_req("/api/v1/agenthub/published/nope"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // ---- stats ----

    #[tokio::test]
    async fn stats_returns_counts_without_panic() {
        let h = AgentHubRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/agenthub/stats")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body["total_agents"].as_u64().unwrap() >= 8);
        assert!(resp.body["installed"].is_u64());
        assert!(resp.body["toolchains_ready"].is_u64());
        assert!(resp.body["tasks"].is_u64());
    }

    // ---- 兜底 ----

    #[tokio::test]
    async fn unmatched_route_returns_404() {
        let h = AgentHubRouteHandler::new();
        let resp = h.handle(get_req("/api/v1/agenthub/nope")).await.unwrap();
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<AgentHubRouteHandler>();
    }

    // ---- Web 界面（mock 启动器：fake 进程/端口探针，绝不真跑 opencode）----

    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Mutex as StdMutex;

    /// mock 启动器：spawn 假装成功（返回递增假 pid，同步回调一行假日志），
    /// `mark_port` 时把声明端口标为开（就绪成功路径；None = 端口永不开，
    /// 驱动启动失败路径）；kill/kill_port 把端口标回关；port_open 查开端口集合；
    /// pid_alive = 未被 kill 过。
    struct MockWebLauncher {
        open_ports: StdMutex<std::collections::HashSet<u16>>,
        spawned: StdMutex<Vec<String>>,
        killed: StdMutex<Vec<u32>>,
        killed_ports: StdMutex<Vec<u16>>,
        mark_port: Option<u16>,
        next_pid: AtomicU32,
        /// true = kill(pid) 杀掉记账 pid 但端口保持开（模拟 opencode npm 包
        /// bin 为 shim：真实服务进程是其子进程，接管端口）——驱动 fuser 兜底。
        kill_keeps_port: bool,
    }

    impl MockWebLauncher {
        /// `mark_port`：spawn 成功后立刻标记为开的端口（None → 永不就绪）。
        fn new(mark_port: Option<u16>) -> Self {
            Self {
                open_ports: StdMutex::new(std::collections::HashSet::new()),
                spawned: StdMutex::new(Vec::new()),
                killed: StdMutex::new(Vec::new()),
                killed_ports: StdMutex::new(Vec::new()),
                mark_port,
                next_pid: AtomicU32::new(4242),
                kill_keeps_port: false,
            }
        }
        fn open(&self, port: u16) {
            self.open_ports.lock().unwrap().insert(port);
        }
        fn spawn_argv(&self) -> Vec<String> {
            self.spawned.lock().unwrap().clone()
        }
    }

    impl WebLauncher for MockWebLauncher {
        fn spawn(
            &self,
            argv: &[String],
            on_line: Arc<dyn Fn(&str) + Send + Sync>,
        ) -> Result<u32, String> {
            self.spawned
                .lock()
                .unwrap()
                .push(argv.to_vec().join(" "));
            if let Some(p) = self.mark_port {
                self.open(p);
            }
            let pid = self.next_pid.fetch_add(1, Ordering::SeqCst);
            on_line(&format!("mock-serve listening on pid {pid}"));
            Ok(pid)
        }
        fn kill(&self, pid: u32) -> Result<(), String> {
            self.killed.lock().unwrap().push(pid);
            if !self.kill_keeps_port {
                self.open_ports.lock().unwrap().clear();
            }
            Ok(())
        }
        fn kill_port(&self, port: u16) -> Result<(), String> {
            self.killed_ports.lock().unwrap().push(port);
            self.open_ports.lock().unwrap().remove(&port);
            Ok(())
        }
        fn port_open(&self, port: u16) -> bool {
            self.open_ports.lock().unwrap().contains(&port)
        }
        fn pid_alive(&self, pid: u32) -> bool {
            !self.killed.lock().unwrap().contains(&pid)
        }
    }

    /// 构造注入 mock 启动器的 handler（300ms 就绪探测 / 20ms 轮询；opencode
    /// 预置条目自带 web 描述符，固定端口 OPENCODE_WEB_PORT）。
    fn web_handler(
        launcher: Arc<MockWebLauncher>,
    ) -> (AgentHubRouteHandler, TempDirGuard) {
        let guard = TempDirGuard::new("web");
        let web = WebState::with_launcher(
            launcher,
            std::time::Duration::from_millis(300),
            std::time::Duration::from_millis(20),
        );
        let h = AgentHubRouteHandler::with_state_file_and_web(&guard.path("agents.json"), web);
        (h, guard)
    }

    fn web_req(method: HttpMethod, path: &str, host: Option<&str>) -> ApiRequest {
        let mut headers = serde_json::json!({});
        if let Some(h) = host {
            headers["Host"] = serde_json::json!(h);
        }
        ApiRequest {
            method,
            path: path.into(),
            headers,
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    #[test]
    fn host_only_strips_port_and_keeps_ipv6() {
        assert_eq!(host_only("192.168.1.5:8558"), "192.168.1.5");
        assert_eq!(host_only("node.example.com"), "node.example.com");
        assert_eq!(host_only("[::1]:8558"), "[::1]");
        assert_eq!(host_only("[fe80::1]"), "[fe80::1]");
        // 非数字后缀不是端口，原样保留
        assert_eq!(host_only("host:bad"), "host:bad");
        assert_eq!(host_only(""), "");
    }

    #[test]
    fn web_base_host_raw_three_branches() {
        // 分支 1：请求 Host 头（去 API 端口；服务端口由 web_url 拼）
        assert_eq!(web_base_host_raw(Some("192.168.1.5:8558"), None), "192.168.1.5");
        assert_eq!(web_base_host_raw(Some("nas.lan"), Some("1.2.3.4")), "nas.lan");
        // 空/空白 Host 视为缺失
        assert_eq!(web_base_host_raw(Some("  "), Some("10.0.0.9:9000")), "10.0.0.9");
        // 分支 2：通告 host（去端口）
        assert_eq!(
            web_base_host_raw(None, Some("spark.example.net:8558")),
            "spark.example.net"
        );
        // 分支 3：都没有 → 回环
        assert_eq!(web_base_host_raw(None, None), "127.0.0.1");
    }

    #[test]
    fn web_url_joins_host_port_path() {
        let desc = AgentWebDesc {
            start_cmd: vec!["opencode".into()],
            port: 4096,
            url_path: "/".into(),
            note: String::new(),
        };
        assert_eq!(web_url("192.168.1.5", &desc), "http://192.168.1.5:4096/");
        let with_path = AgentWebDesc {
            url_path: "/ui/".into(),
            ..desc
        };
        assert_eq!(web_url("[::1]", &with_path), "http://[::1]:4096/ui/");
    }

    #[tokio::test]
    async fn web_start_ready_idempotent_stop_roundtrip() {
        let (h, launcher_holder, _guard) = {
            let l = Arc::new(MockWebLauncher::new(Some(OPENCODE_WEB_PORT)));
            let (h, g) = web_handler(Arc::clone(&l));
            (h, l, g)
        };

        // 未启动 → status：running=false
        let resp = h
            .handle(web_req(
                HttpMethod::Get,
                "/api/v1/agenthub/web/opencode/status",
                Some("nas.lan:8558"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["running"], false);
        assert_eq!(resp.body["url"], serde_json::Value::Null);

        // start：mock spawn 即标端口开 → 就绪成功
        let resp = h
            .handle(web_req(
                HttpMethod::Post,
                "/api/v1/agenthub/web/opencode/start",
                Some("nas.lan:8558"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "start: {resp:?}");
        assert_eq!(resp.body["state"], "started");
        assert_eq!(resp.body["pid"].as_u64(), Some(4242));
        // URL 用 Host 头推导（去 API 端口 8558 → 换服务端口 4096）
        assert_eq!(resp.body["url"], "http://nas.lan:4096/");
        assert_eq!(resp.body["port"].as_u64(), Some(u64::from(OPENCODE_WEB_PORT)));
        assert_eq!(launcher_holder.spawned.lock().unwrap().len(), 1);
        let argv = launcher_holder.spawn_argv()[0].clone();
        assert!(
            argv.starts_with("opencode serve --port"),
            "spawn argv: {argv}"
        );
        assert!(argv.contains("--hostname 0.0.0.0"), "跨机访问需绑 0.0.0.0: {argv}");

        // 幂等：再次 start → 不二次 spawn，同一 URL
        let resp = h
            .handle(web_req(
                HttpMethod::Post,
                "/api/v1/agenthub/web/opencode/start",
                Some("nas.lan:8558"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["state"], "idempotent");
        assert_eq!(resp.body["url"], "http://nas.lan:4096/");
        assert_eq!(
            launcher_holder.spawned.lock().unwrap().len(),
            1,
            "幂等不应二次 spawn"
        );

        // status：running + started_at + 启动命令行在日志尾
        let resp = h
            .handle(web_req(
                HttpMethod::Get,
                "/api/v1/agenthub/web/opencode/status",
                Some("nas.lan"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["running"], true);
        assert_eq!(resp.body["pid"].as_u64(), Some(4242));
        assert!(
            resp.body["started_at"].as_str().is_some(),
            "started_at: {resp:?}"
        );
        let tail = resp.body["log_tail"].as_str().unwrap_or("");
        assert!(tail.contains("opencode serve"), "日志尾应含启动命令: {tail}");

        // stop → 200；再 stop → 404（未在运行）
        let resp = h
            .handle(web_req(
                HttpMethod::Post,
                "/api/v1/agenthub/web/opencode/stop",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "stop: {resp:?}");
        assert_eq!(resp.body["ok"], true);
        assert_eq!(launcher_holder.killed.lock().unwrap().as_slice(), [4242u32]);
        let resp = h
            .handle(web_req(
                HttpMethod::Post,
                "/api/v1/agenthub/web/opencode/stop",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404, "重复 stop 应 404");
    }

    #[tokio::test]
    async fn web_stop_falls_back_to_kill_port_for_shim_processes() {
        // opencode npm 包 bin 为 shim（实测：spawn pid 312617 ≠ 持端口 pid
        // 312619）——杀记账 pid 后端口仍开 → fuser 按端口兜底才算停干净
        let (h, launcher_holder, _guard) = {
            let mut m = MockWebLauncher::new(Some(OPENCODE_WEB_PORT));
            m.kill_keeps_port = true; // kill(pid) 不关端口（子进程接管）
            let l = Arc::new(m);
            let (h, g) = web_handler(Arc::clone(&l));
            (h, l, g)
        };
        let resp = h
            .handle(web_req(
                HttpMethod::Post,
                "/api/v1/agenthub/web/opencode/start",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "start: {resp:?}");

        let resp = h
            .handle(web_req(
                HttpMethod::Post,
                "/api/v1/agenthub/web/opencode/stop",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "shim 场景 stop 应经 fuser 兜底成功: {resp:?}");
        assert_eq!(launcher_holder.killed.lock().unwrap().as_slice(), [4242u32]);
        assert_eq!(
            launcher_holder.killed_ports.lock().unwrap().as_slice(),
            [OPENCODE_WEB_PORT],
            "应按端口兜底杀掉接管端口的子进程"
        );
    }

    #[tokio::test]
    async fn web_start_failure_port_never_ready() {
        let (h, launcher_holder, _guard) = {
            let l = Arc::new(MockWebLauncher::new(None)); // 端口永不开
            let (h, g) = web_handler(Arc::clone(&l));
            (h, l, g)
        };
        // pid 恒活 + 端口永不就绪 → 探测超时（300ms 注入值）→ 500 + kill
        let resp = h
            .handle(web_req(
                HttpMethod::Post,
                "/api/v1/agenthub/web/opencode/start",
                Some("127.0.0.1:8558"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 500, "端口未就绪应 500: {resp:?}");
        let err = resp.body["error"].as_str().unwrap();
        assert!(err.contains("未就绪"), "错误应说明端口未就绪: {err}");
        assert_eq!(launcher_holder.killed.lock().unwrap().len(), 1, "失败应 kill 子进程");
        // 状态回到未运行（表条目已清）
        let resp = h
            .handle(web_req(
                HttpMethod::Get,
                "/api/v1/agenthub/web/opencode/status",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.body["running"], false);
    }

    #[tokio::test]
    async fn web_status_rebuilds_table_from_live_port() {
        // os-api 重启后表丢失、子进程存活（端口开）：start 直接按端口重建表
        // （recovered，pid 不可知不二次 spawn）；status 同样恢复；stop 走 kill_port
        let (h, launcher_holder, _guard) = {
            let l = Arc::new(MockWebLauncher::new(None));
            l.open(OPENCODE_WEB_PORT); // 模拟重启残留的监听进程
            let (h, g) = web_handler(Arc::clone(&l));
            (h, l, g)
        };

        // start（表空但端口开）→ 恢复态，不 spawn
        let resp = h
            .handle(web_req(
                HttpMethod::Post,
                "/api/v1/agenthub/web/opencode/start",
                Some("10.1.2.3:8558"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "start: {resp:?}");
        assert_eq!(resp.body["state"], "recovered");
        assert_eq!(resp.body["pid"], serde_json::Value::Null, "恢复条目 pid 不可知");
        assert_eq!(resp.body["url"], "http://10.1.2.3:4096/");
        assert_eq!(
            launcher_holder.spawned.lock().unwrap().len(),
            0,
            "端口已占不应二次 spawn"
        );

        // status：running=true，pid/started_at 如实为 null
        let resp = h
            .handle(web_req(
                HttpMethod::Get,
                "/api/v1/agenthub/web/opencode/status",
                Some("10.1.2.3:8558"),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["running"], true);
        assert_eq!(resp.body["pid"], serde_json::Value::Null);
        assert_eq!(resp.body["started_at"], serde_json::Value::Null);

        // stop：pid 不可知 → kill_port（fuser 路径）
        let resp = h
            .handle(web_req(
                HttpMethod::Post,
                "/api/v1/agenthub/web/opencode/stop",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "stop: {resp:?}");
        assert_eq!(
            launcher_holder.killed_ports.lock().unwrap().as_slice(),
            [OPENCODE_WEB_PORT]
        );
    }

    #[tokio::test]
    async fn web_status_reconciles_dead_entry() {
        // 表内条目但端口已关（进程被外部杀/自行退出）→ status 清死条目 running=false；
        // 随后 start 正常走真实 spawn
        let (h, launcher_holder, _guard) = {
            let l = Arc::new(MockWebLauncher::new(Some(OPENCODE_WEB_PORT)));
            let (h, g) = web_handler(Arc::clone(&l));
            (h, l, g)
        };
        // 起 → 手工模拟进程死亡（端口关 + pid 被 kill 记账）→ 表条目仍在
        h.handle(web_req(HttpMethod::Post, "/api/v1/agenthub/web/opencode/start", None))
            .await
            .unwrap();
        launcher_holder.kill(4242).expect("mock kill");

        let resp = h
            .handle(web_req(
                HttpMethod::Get,
                "/api/v1/agenthub/web/opencode/status",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.body["running"], false, "死条目应清理: {resp:?}");

        // 再次 start 走全新 spawn（idempotent 之外的路径）
        let resp = h
            .handle(web_req(
                HttpMethod::Post,
                "/api/v1/agenthub/web/opencode/start",
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["state"], "started");
        assert_eq!(launcher_holder.spawned.lock().unwrap().len(), 2, "死条目后应重新 spawn");
    }

    #[tokio::test]
    async fn web_endpoints_reject_non_web_and_unknown_agents() {
        let (h, _guard) = {
            let l = Arc::new(MockWebLauncher::new(None));
            let (h, g) = web_handler(l);
            (h, g)
        };
        // 未知 agent → 404
        for (m, suffix) in [
            (HttpMethod::Post, "start"),
            (HttpMethod::Post, "stop"),
            (HttpMethod::Get, "status"),
        ] {
            let resp = h
                .handle(web_req(
                    m,
                    &format!("/api/v1/agenthub/web/__nope__/{suffix}"),
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 404, "{suffix} 未知 agent 应 404");
        }
        // 已知但未标注 web（Claude Code）→ 400（诚实不猜：未实测的不标）
        for (m, suffix) in [
            (HttpMethod::Post, "start"),
            (HttpMethod::Post, "stop"),
            (HttpMethod::Get, "status"),
        ] {
            let resp = h
                .handle(web_req(
                    m,
                    &format!("/api/v1/agenthub/web/claude-code/{suffix}"),
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 400, "{suffix} 无 web 描述符应 400: {resp:?}");
        }
    }

    #[tokio::test]
    async fn web_desc_survives_persisted_json_roundtrip_without_web_key() {
        // 旧持久化文件（无 web 键）读回不炸：#[serde(default)] 兜 None
        let guard = TempDirGuard::new("web-old");
        let path = guard.path("agents.json");
        std::fs::write(
            &path,
            r#"{"agents":[{"id":"custom-1","name":"Old","description":"","category":"custom","icon":"🧩","source":"user","install_type":"npm","install_target":"x","check_binary":"x","homepage":"","publisher":"用户发布","tags":[],"installed":false}],"seq":1}"#,
        )
        .expect("写旧格式状态");
        let h = AgentHubRouteHandler::with_state_file(&path);
        let resp = h
            .handle(get_req("/api/v1/agenthub/agents/custom-1"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert!(
            resp.body.get("web").is_none(),
            "旧条目 web 应缺省 None: {resp:?}"
        );
    }
}
