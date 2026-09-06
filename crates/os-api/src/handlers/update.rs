//! `UpdateRouteHandler` —— 「更新」桌面应用的 HTTP 适配器：系统版本 /
//! 更新通道 / 可用版本检查 / 更新任务（A/B 槽位视图 + 状态机）管理。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/update/*`）翻译为 os-update 编排，
//! 返回 JSON。这是 OS「更新」桌面应用的后端 REST 入口。
//!
//! # 更新源（本生态闭环，两级解析链）
//!
//! 1. **本地裸仓库**（缺省 `/tank/git-repos/nexos.git`，env `NEXOS_UPDATE_REPO`
//!    覆盖）——NexHub 发版即 git tag（形如 `v0.1.0`），`POST /check` 用
//!    `git for-each-ref refs/tags` 子进程读 tag 列表（tag 名 + 打 tag 时间）。
//!    有本地副本的节点（联邦 auto-pull 跟随，`NEXOS_LOBBY_AUTO_PULL`）走本级。
//! 2. **远端 git URL**（env `NEXOS_UPDATE_REPO_URL`，http(s) 如
//!    `http://203.0.113.2:8558/git/nexos.git`）——本地路径缺失/无 tag 时用
//!    `git ls-remote --tags --refs <url>` 纯网络查询（无需本地克隆；install.sh
//!    装的新节点无 `/tank`，开箱即由安装源节点写入该 env）。ls-remote 拿不到
//!    creatordate，`created_at` 为 None。
//! 3. 两者都失败（仓库不存在 / git 不可用 / 无 tag / URL 不可达）→ 空清单不
//!    报错（`repo_reachable:false` 降级，前端按三态提示：本地模式 / 远端
//!    git 模式 / 均不可达）。semver 比较复用 [`os_update::version`]。
//!
//! # 更新通道（"留好的更新通道"）
//!
//! 通道即 tag 过滤策略，四选一（`POST /channel` 切换并持久化）：
//! - `stable`：仅正式 tag（排除一切预发布 `x.y.z-*`）；
//! - `beta`：仅 `*-beta*` tag（预发布标识含 `beta`）；
//! - `nightly`：任意最新 tag（含全部预发布）；
//! - `manual`：不自动检查，仅手动触发（过滤同 nightly 全收；当前实现里
//!   `POST /check` 本身就是手动触发，未来接入自动轮询时跳过本通道）。
//!
//! # A/B 槽位（内存态，重启重建）
//!
//! 复用 [`os_update::slot::SlotManager`] 纯状态机：初始化 A=active（当前
//! 版本）、B=inactive（更新写入目标）。`GET /status` 返回两槽视图 +
//! active/writable 槽结论。本期不执行真实写槽（见下），槽位视图用于
//! 前端展示与后续真实 I/O 接入时的决策底座。
//!
//! # 工件登记（apply 的前置：Files API 闭环）
//!
//! 本机更新工件（os-api 二进制）经 Files API 上传到本机后，先调
//! `POST /artifact {version, path}` 登记：path 为本机绝对路径，校验
//! 存在 / 可读 / ≥1MB / ELF 魔数（`\x7fELF` 头），version 须 semver；
//! 登记时算好 sha256 一并落盘（重复 version 覆盖）。apply 只安装**已登记**
//! 工件——版本清单（NexHub tag）负责"发现新版本"，Files API + 工件登记
//! 负责"把二进制运到本机"，apply 负责"装上并自重启"。
//!
//! 第二条登记路径（2026-09-03）：provisioning 的 prepare-distributable
//! 成功后经共享实例调 [`UpdateRouteHandler::register_artifact_and_persist`]
//! **自动登记同版本工件**（version=运行二进制 `CARGO_PKG_VERSION`、
//! path=分发产物 latest-os-api.bin，与手动 POST /artifact 同一校验/sha/
//! 幂等语义）——发版流程"三节点各跑一次 prepare"即同时喂饱 dist 下载与
//! 页内 apply 两条更新通道（docs/UPDATE_APP.md §1a）。
//!
//! # 更新任务状态机（真实安装管线）
//!
//! `POST /apply` 前置校验（版本合法且新于当前 + 已登记对应工件）后建任务，
//! 按 `pending → verifying → writing → reboot_pending → done` 推进
//! （`failed` 为失败终态），`GET /tasks/:id` 每次轮询推进一步：
//!
//! - `verifying`：对登记工件**真实复核**——重算 sha256 与登记值比对 + ELF
//!   魔数复核（防登记后文件被替换/截断）；任一不过 → failed 带原因。
//! - `writing`：**真实安装**（防呆见 [`UpdateRouteHandler::install_artifact`]）：
//!   1. `current_exe()` 推导 exec_dir（失败 → failed 带原因，绝不盲装）；
//!   2. 工件 `fs::copy` 到 `<exec_dir>/os-api.staged` + chmod 755；
//!   3. 备份当前二进制为 `os-api.bak-<ts>`（保留最近 3 个，多余的清掉）；
//!   4. `rename` staged → 当前二进制路径（Linux 对运行中二进制 rename-over
//!      合法——内核只挡 open-for-write，ETXTBSY 不适用于 rename）；
//!   5. 置 `reboot_pending`（note 声明服务将自重启），并 spawn 分离进程
//!      `sh -c "sleep 1; systemctl restart os-api || systemctl restart
//!      nexos-os-api"` 自重启——os-api 由 systemd 管理，重启命令从外部
//!      systemd 触发对任意 Restart 策略都成立；服务名不匹配的部署上
//!      fallback 失败仅记日志，人工重启兜底。
//!
//! 任务推进方式：`GET /tasks/:id` 每次轮询推进一步（前端进度条据此呈现），
//! 任务列表内存 + JSON 持久化（重启后历史可见，非终态任务重启后停在原状态）。
//!
//! # 持久化
//!
//! JSON 文件（env `NEXOS_UPDATE_STATE`，缺省 `/tank/os-data/update-state.json`），
//! 目录不存在自动创建，**原子写**（先写 `.tmp` 再 rename）。内容：当前通道、
//! 上次检查时间、待应用清单、工件列表（version/path/size/sha256/registered_at）、
//! 任务列表（`GET /history` 从任务过滤 done/reboot_pending）。
//!
//! # 鉴权
//!
//! 读端点（GET）公开；写端点（POST）admin（网关 Bearer 认证）。
//!
//! # 路由表（10 条）
//!
//! | method | path                          | 动作 |
//! |--------|-------------------------------|------|
//! | GET    | `/api/v1/update/status`       | 当前版本/通道/槽位视图/上次检查 |
//! | GET    | `/api/v1/update/channels`     | 通道列表 + 当前通道 |
//! | POST   | `/api/v1/update/channel`      | 切换通道（admin，持久化）|
//! | POST   | `/api/v1/update/check`        | 检查更新（admin，两级源解析链读 tag）|
//! | POST   | `/api/v1/update/artifact`    | 登记更新工件（admin；Files API 上传后调用）|
//! | GET    | `/api/v1/update/artifacts`   | 已登记工件列表 |
//! | POST   | `/api/v1/update/apply`        | 建更新任务并真实安装（admin）|
//! | GET    | `/api/v1/update/tasks`        | 任务列表 |
//! | GET    | `/api/v1/update/tasks/:id`    | 任务详情（轮询即推进）|
//! | GET    | `/api/v1/update/history`      | 已应用历史 |

use std::sync::Mutex;

use async_trait::async_trait;
use os_update::slot::SlotManager;
use os_update::update::UpdateSlot;
use os_update::version::Version;
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

// ----------------------------------------------------------------------------
// 常量：通道定义 / 缺省路径 / 预留说明
// ----------------------------------------------------------------------------

/// 通道标识。
pub const CHANNEL_STABLE: &str = "stable";
/// 通道标识。
pub const CHANNEL_BETA: &str = "beta";
/// 通道标识。
pub const CHANNEL_NIGHTLY: &str = "nightly";
/// 通道标识。
pub const CHANNEL_MANUAL: &str = "manual";

/// 全部合法通道。
pub const ALL_CHANNELS: [&str; 4] = [
    CHANNEL_STABLE,
    CHANNEL_BETA,
    CHANNEL_NIGHTLY,
    CHANNEL_MANUAL,
];

/// 通道元信息（id + 展示名 + 一句话说明）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelInfo {
    /// 通道 id（stable/beta/nightly/manual）
    pub id: String,
    /// 展示名
    pub name: String,
    /// 一句话说明
    pub description: String,
}

/// 通道目录（`GET /channels` 数据源）。
#[must_use]
pub fn channel_catalog() -> Vec<ChannelInfo> {
    vec![
        ChannelInfo {
            id: CHANNEL_STABLE.into(),
            name: "正式版".into(),
            description: "仅正式发版 tag（排除一切预发布），适合生产节点".into(),
        },
        ChannelInfo {
            id: CHANNEL_BETA.into(),
            name: "公测版".into(),
            description: "仅 *-beta* tag，提前体验新功能".into(),
        },
        ChannelInfo {
            id: CHANNEL_NIGHTLY.into(),
            name: "每夜版".into(),
            description: "任意最新 tag（含全部预发布），跟进最快风险最高".into(),
        },
        ChannelInfo {
            id: CHANNEL_MANUAL.into(),
            name: "手动模式".into(),
            description: "不自动检查，仅手动触发（过滤同每夜版全收）".into(),
        },
    ]
}

/// 更新源裸仓库缺省路径（NexHub 发版 tag 所在；env `NEXOS_UPDATE_REPO` 覆盖）。
pub const DEFAULT_REPO_PATH: &str = "/tank/git-repos/nexos.git";

/// 远端更新源 env 名（http(s) git URL，如 `http://203.0.113.2:8558/git/nexos.git`；
/// 本地裸仓库缺失/无 tag 时的替代更新源，`git ls-remote --tags` 纯网络查询）。
pub const UPDATE_REPO_URL_ENV: &str = "NEXOS_UPDATE_REPO_URL";

/// `git ls-remote` 网络查询超时（秒）：URL 不可达（对端挂/防火墙吞包）时
/// ls-remote 会挂到 TCP 超时，封顶避免 check 请求长时间悬挂。
const LS_REMOTE_TIMEOUT_SECS: u64 = 15;

/// 状态持久化缺省路径（env `NEXOS_UPDATE_STATE` 覆盖）。
pub const DEFAULT_STATE_PATH: &str = "/tank/os-data/update-state.json";

/// 工件最小体积（1MB）：低于此值视为残留/半传文件拒绝登记（真实 os-api
/// release 二进制远大于此；阈值防呆，不构成正确性依据）。
pub const MIN_ARTIFACT_BYTES: u64 = 1024 * 1024;

/// staged 暂存文件名（写入 exec_dir，rename 前的落点）。
const STAGED_NAME: &str = "os-api.staged";

/// 备份文件名前缀（`os-api.bak-<时间戳>`，时间戳格式字典序即时间序）。
const BACKUP_PREFIX: &str = "os-api.bak-";

/// 备份保留个数（多余的清理）。
const BACKUP_KEEP: usize = 3;

/// writing 完成后的自重启说明（写入任务 note，前端原样展示）。
const RESTART_NOTE: &str = "已写入，服务将在数秒内自重启";

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 一条可用更新（check 结果项，也出现在 /status 的待应用清单里）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AvailableUpdate {
    /// NexHub 仓库里的 tag 名（如 `v0.2.0`）
    pub tag: String,
    /// 解析后的 semver 版本（`0.2.0`）
    pub version: String,
    /// 该 tag 归属的通道桶：`stable` / `beta` / `prerelease`（其它预发布）
    pub channel: String,
    /// 打 tag 时间（git creatordate，解析失败为 None）
    pub created_at: Option<String>,
}

/// 更新任务（apply 创建；GET /tasks* 轮询推进）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateTask {
    /// 任务 ID（`update-N`）
    pub id: String,
    /// 目标版本
    pub version: String,
    /// 对应 NexHub tag（apply 时可从最近一次 check 结果反查；未知为 None）
    pub tag: Option<String>,
    /// 发起任务时的通道
    pub channel: String,
    /// 状态：pending / verifying / writing / reboot_pending / done / failed
    pub status: String,
    /// 写入目标槽（A/B 双槽语义下始终为 `b`——另一槽）
    pub slot_target: String,
    /// 进度（0-100，按状态机阶段推进的启发值）
    pub progress: u8,
    /// 创建时间（ISO）
    pub created_at: String,
    /// 最后更新时间（ISO）
    pub updated_at: String,
    /// 失败原因（failed 时）
    pub error: Option<String>,
    /// 写入阶段说明（writing→reboot_pending 写入自重启声明）
    pub note: Option<String>,
    /// 工件本机路径（建任务时从登记快照；旧版持久化任务无此字段）
    #[serde(default)]
    pub artifact_path: Option<String>,
    /// 工件 sha256（登记时算好；verifying 阶段重算比对）
    #[serde(default)]
    pub artifact_sha256: Option<String>,
}

/// 已登记的更新工件（POST /artifact 登记项；GET /artifacts 列表项）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateArtifact {
    /// 版本（semver，登记时归一化存储）
    pub version: String,
    /// 本机绝对路径（Files API 上传产物）
    pub path: String,
    /// 体积（字节，登记时快照）
    pub size: u64,
    /// sha256（登记时算好；apply verifying 阶段重算比对）
    pub sha256: String,
    /// 登记时间（ISO）
    pub registered_at: String,
}

/// `GET /status` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateStatusView {
    /// 当前系统版本
    pub current_version: String,
    /// 当前更新通道
    pub channel: String,
    /// 槽 A 状态（os-update SlotState 序列化：slot/status/version/时间戳）
    pub slot_a: serde_json::Value,
    /// 槽 B 状态
    pub slot_b: serde_json::Value,
    /// 当前活动槽（"a"/"b"）
    pub active_slot: String,
    /// 更新写入目标槽（"a"/"b"）
    pub writable_slot: String,
    /// 上次检查更新时间（None = 从未检查）
    pub last_check: Option<String>,
    /// 待应用清单（上次 check 的可用版本）
    pub pending_updates: Vec<AvailableUpdate>,
    /// 状态持久化路径（None = 内存态）
    pub state_path: Option<String>,
}

/// `POST /check` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    /// 当前系统版本
    pub current_version: String,
    /// 检查所用通道
    pub channel: String,
    /// 可用版本清单（按版本降序；源不可达/无 tag 为空）
    pub available: Vec<AvailableUpdate>,
    /// 本次检查时间（ISO）
    pub checked_at: String,
    /// 更新源描述：实际采用的源（本地裸仓库路径或远端 git URL）；均不可达
    /// 时为本地仓库路径（与历史版本口径一致）
    pub repo: String,
    /// 更新源模式三态：`local`（本地裸仓库）/ `remote`（远端 git URL）/
    /// `none`（本地与远端均不可达，降级空清单）
    pub repo_mode: String,
    /// 配置的远端更新源 URL（env `NEXOS_UPDATE_REPO_URL`；未配置为 None）
    /// ——前端"均不可达"提示与存量节点手工配置指引用
    #[serde(default)]
    pub repo_url: Option<String>,
    /// 更新源是否真实可读（false = 降级：本地与远端均失败——仓库不存在 /
    /// git 失败 / URL 不可达 / 无 tag）
    pub repo_reachable: bool,
}

// ----------------------------------------------------------------------------
// 持久化状态（JSON 原子写）
// ----------------------------------------------------------------------------

/// 落盘的更新状态（通道 + 上次检查 + 工件列表 + 任务列表）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistState {
    /// 当前通道（缺省 stable）
    channel: String,
    /// 上次检查时间
    last_check: Option<String>,
    /// 上次 check 的可用清单（/status 的待应用清单）
    available: Vec<AvailableUpdate>,
    /// 已登记工件（旧版 JSON 无此字段 → 空表）
    #[serde(default)]
    artifacts: Vec<UpdateArtifact>,
    /// 全部任务（含历史；/history 过滤 done/reboot_pending）
    tasks: Vec<UpdateTask>,
}

impl Default for PersistState {
    fn default() -> Self {
        Self {
            channel: CHANNEL_STABLE.into(),
            last_check: None,
            available: Vec::new(),
            artifacts: Vec::new(),
            tasks: Vec::new(),
        }
    }
}

/// 原子写 JSON（先写 `<path>.tmp` 再 rename；父目录不存在自动创建）。
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

/// 读回 JSON 状态（缺失/解析失败 → 缺省，不报错：首次运行/文件损坏降级）。
fn load_state_from(path: &str) -> PersistState {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
        Err(_) => PersistState::default(),
    }
}

// ----------------------------------------------------------------------------
// 纯函数：tag 解析 / 通道过滤 / 可用清单
// ----------------------------------------------------------------------------

/// 解析 NexHub tag → semver（剥前导 `v`/`V`；非 semver 返回 None）。
///
/// `v0.2.0` / `0.2.0` / `V0.2.0` → `0.2.0`；`windows-m1` / `release-2026` → None。
#[must_use]
pub fn parse_tag(tag: &str) -> Option<Version> {
    let t = tag.trim();
    let core = t
        .strip_prefix('v')
        .or_else(|| t.strip_prefix('V'))
        .unwrap_or(t);
    Version::parse(core).ok()
}

/// 判定版本归属的通道桶（check 结果展示用）。
#[must_use]
pub fn tag_bucket(v: &Version) -> &'static str {
    match &v.pre {
        None => CHANNEL_STABLE,
        Some(p) if p.contains("beta") => CHANNEL_BETA,
        Some(_) => "prerelease",
    }
}

/// 通道过滤：给定通道，该版本是否可见。
///
/// - stable：仅正式版（无预发布标识）；
/// - beta：仅预发布标识含 `beta` 的版本；
/// - nightly / manual：全收。
#[must_use]
pub fn channel_allows(channel: &str, v: &Version) -> bool {
    match channel {
        CHANNEL_STABLE => v.pre.is_none(),
        CHANNEL_BETA => v.pre.as_deref().is_some_and(|p| p.contains("beta")),
        CHANNEL_NIGHTLY | CHANNEL_MANUAL => true,
        _ => false,
    }
}

/// 从 tag 列表（tag 名 + 可选时间）计算可用更新清单。
///
/// 规则：tag 能解析为 semver 且 **严格新于** current 且通过通道过滤；
/// 结果按版本降序（最新在前）。
#[must_use]
pub fn filter_available(
    tags: &[(String, Option<String>)],
    current: &Version,
    channel: &str,
) -> Vec<AvailableUpdate> {
    let mut out: Vec<(Version, AvailableUpdate)> = tags
        .iter()
        .filter_map(|(tag, at)| {
            let v = parse_tag(tag)?;
            if &v <= current || !channel_allows(channel, &v) {
                return None;
            }
            Some((
                v.clone(),
                AvailableUpdate {
                    tag: tag.clone(),
                    version: v.as_string(),
                    channel: tag_bucket(&v).to_string(),
                    created_at: at.clone(),
                },
            ))
        })
        .collect();
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.into_iter().map(|(_, u)| u).collect()
}

/// 解析 `git ls-remote --tags` 输出 → tag 列表（tag 名 + None 时间）。
///
/// 输出形态：每行 `<sha1>\trefs/tags/<tag>`；annotated tag 会额外多出一行
/// `<sha>\trefs/tags/<tag>^{}`（peeled 指向提交对象）——即便命令带 `--refs`
/// 也**防御性过滤** `^{}` 行（旧版 git 不支持 --refs 时防重复 tag）。
/// `refs/tags/` 前缀剥除后即 tag 名；空行/形态异常的行忽略。
/// ls-remote 协议不携带 creatordate，时间恒 None（`filter_available`
/// 的通道/semver 过滤与本地 tag 同一口径，仅 `created_at` 展示为空）。
#[must_use]
pub fn parse_ls_remote_output(raw: &str) -> Vec<(String, Option<String>)> {
    raw.lines()
        .filter_map(|line| {
            let tag = line.split('\t').nth(1)?.trim().strip_prefix("refs/tags/")?;
            if tag.is_empty() || tag.ends_with("^{}") {
                return None;
            }
            Some((tag.to_string(), None))
        })
        .collect()
}

/// 计算文件 sha256（64KB 分块流式；错误带路径上下文）。
fn sha256_file(path: &std::path::Path) -> Result<String, String> {
    use sha2::Digest;
    use std::io::Read;
    let mut f =
        std::fs::File::open(path).map_err(|e| format!("打开工件失败 {}: {e}", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f
            .read(&mut buf)
            .map_err(|e| format!("读工件失败 {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// ELF 魔数校验（头 4 字节 == `\x7fELF`；不足 4 字节视为非 ELF）。
fn elf_magic_ok(path: &std::path::Path) -> Result<bool, String> {
    use std::io::Read;
    let mut f =
        std::fs::File::open(path).map_err(|e| format!("打开工件失败 {}: {e}", path.display()))?;
    let mut head = [0u8; 4];
    match f.read_exact(&mut head) {
        Ok(()) => Ok(head == [0x7f, b'E', b'L', b'F']),
        Err(_) => Ok(false),
    }
}

/// 清理过期备份：`<dir>` 内 `os-api.bak-*` 只保留最近 `keep` 个
/// （备份名含 `%Y%m%dT%H%M%S` 时间戳，字典序即时间序；删除失败记日志不阻塞）。
fn prune_backups(dir: &std::path::Path, keep: usize) {
    let mut baks: Vec<std::path::PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(BACKUP_PREFIX))
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    baks.sort();
    while baks.len() > keep {
        let old = baks.remove(0);
        if let Err(e) = std::fs::remove_file(&old) {
            eprintln!("[update] 清理过期备份失败 {}: {e}", old.display());
        }
    }
}

/// spawn 分离的自重启进程：`sh -c "sleep 1; systemctl restart os-api ||
/// systemctl restart nexos-os-api"`。
///
/// 进程模型：os-api 由 systemd 管理，重启命令**从外部 systemd 触发**——
/// 服务自身不能 fork-exec 替换自己（systemd 会按 MainPID 判定服务退出并按
/// Restart 策略拉起，二进制此时可能已被 rename 换新），外部 restart 对任意
/// Restart 策略都成立。分离要点：spawn 后立即丢弃 child 句柄（不 wait），
/// 进程被 reparent 到 init，os-api 退出不影响 sleep/restart 继续；stdout/
/// stderr 置 null（无终端可写）。sleep 1 给当前 HTTP 响应留出返回窗口。
///
/// 服务名兜底：开发机 systemd 单元名是 `os-api`；其他部署若叫
/// `nexos-os-api` 则 fallback；两个都不匹配的部署上本命令失败仅日志可见，
/// 此时人工 `systemctl restart` 兜底（二进制已换新，重启即生效）。
fn spawn_self_restart() {
    use std::process::{Command, Stdio};
    let spawned = Command::new("sh")
        .arg("-c")
        .arg("sleep 1; systemctl restart os-api || systemctl restart nexos-os-api")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match spawned {
        Ok(_child) => eprintln!("[update] 已调度自重启（1s 后 systemctl restart os-api）"),
        Err(e) => eprintln!("[update] 自重启调度失败（需人工 systemctl restart）: {e}"),
    }
}

/// chmod 755（unix）：staged 工件落点须可执行，rename 后直接可跑。
fn set_exec_perm(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("设置执行权限失败 {}: {e}", path.display()))
}

// ----------------------------------------------------------------------------
// UpdateRouteHandler
// ----------------------------------------------------------------------------

/// 「更新」路由处理器——HTTP 边界适配到 os-update 版本/通道/槽位编排。
pub struct UpdateRouteHandler {
    /// NexHub 裸仓库路径（更新源第一级；check 读其 tag 列表）
    repo_path: String,
    /// 远端更新源 URL（env `NEXOS_UPDATE_REPO_URL`；本地裸仓库缺失/无 tag
    /// 时的第二级，`git ls-remote --tags` 纯网络查询；未配置为 None）
    repo_url: Option<String>,
    /// 状态 JSON 路径（None = 纯内存态，测试用）
    state_path: Option<String>,
    /// 当前系统版本（env NEXOS_VERSION 优先，缺省 os-api 包版本）
    current_version: String,
    /// 持久化状态（通道 + 上次检查 + 工件 + 任务）
    state: Mutex<PersistState>,
    /// A/B 槽位状态机（内存态，重启重建：A=active 当前版本、B=inactive）
    slots: Mutex<SlotManager>,
    /// 任务 id 计数器
    counter: Mutex<u64>,
    /// 安装目标（「当前二进制」路径）注入：None = 生产走 current_exe()；
    /// 测试注入临时目录路径，**绝不触碰真实测试进程二进制**（红线）。
    exec_override: Mutex<Option<std::path::PathBuf>>,
    /// 是否允许 writing 完成后 spawn systemctl 自重启。红线：**只有生产构造
    /// `new()` 置 true**——测试构造（with_config）恒 false，绝不真重启开发机
    /// 的 os-api 服务。
    self_restart: bool,
}

impl UpdateRouteHandler {
    /// 生产构造：路径与版本全部 env 驱动（`NEXOS_UPDATE_STATE` /
    /// `NEXOS_UPDATE_REPO` / `NEXOS_UPDATE_REPO_URL` / `NEXOS_VERSION`），并读回
    /// 持久化状态。安装目标走 `current_exe()` 推导；writing 完成后允许自重启。
    #[must_use]
    pub fn new() -> Self {
        let state_path = std::env::var("NEXOS_UPDATE_STATE")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_STATE_PATH.to_string());
        let repo_path = std::env::var("NEXOS_UPDATE_REPO")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_REPO_PATH.to_string());
        let repo_url = std::env::var(UPDATE_REPO_URL_ENV)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let current_version = current_version_from_env();
        let state = load_state_from(&state_path);
        Self::from_parts(
            Some(state_path),
            repo_path,
            repo_url,
            current_version,
            state,
            true,
        )
    }

    /// 测试构造：显式状态路径（None = 纯内存）/ 仓库路径 / 当前版本，
    /// **无远端更新源**（仅本地一级；自重启恒关闭——红线：测试绝不
    /// systemctl restart 开发机服务）。
    #[must_use]
    pub fn with_config(
        state_path: Option<String>,
        repo_path: impl Into<String>,
        current_version: impl Into<String>,
    ) -> Self {
        Self::with_config_and_remote(state_path, repo_path, None, current_version)
    }

    /// 测试构造（两级源解析链）：显式远端更新源 URL（None = 未配置，
    /// 等价 [`Self::with_config`]）。**自重启恒关闭**（红线：测试绝不
    /// systemctl restart 开发机服务）；安装目标默认 current_exe()，需真实
    /// 安装的测试另行注入临时目录。
    #[must_use]
    pub fn with_config_and_remote(
        state_path: Option<String>,
        repo_path: impl Into<String>,
        repo_url: Option<String>,
        current_version: impl Into<String>,
    ) -> Self {
        let state = match &state_path {
            Some(p) => load_state_from(p),
            None => PersistState::default(),
        };
        Self::from_parts(
            state_path,
            repo_path.into(),
            repo_url,
            current_version.into(),
            state,
            false,
        )
    }

    /// 组装（共用：构造 SlotManager 内存态）。`self_restart` 仅生产为 true。
    fn from_parts(
        state_path: Option<String>,
        repo_path: String,
        repo_url: Option<String>,
        current_version: String,
        state: PersistState,
        self_restart: bool,
    ) -> Self {
        let slots = SlotManager::new(UpdateSlot::A, current_version.clone(), chrono::Utc::now());
        Self {
            repo_path,
            repo_url,
            state_path,
            current_version,
            state: Mutex::new(state),
            slots: Mutex::new(slots),
            counter: Mutex::new(0),
            exec_override: Mutex::new(None),
            self_restart,
        }
    }

    /// 测试专用：注入「当前二进制」路径（安装目标指向临时目录）。
    /// 红线：生产代码不得调用；测试若不注入而走到 writing，rename 会替换
    /// **测试进程自身**的 cargo test 二进制——端到端测试必须先注入。
    #[cfg(test)]
    fn set_exec_path_for_test(&self, p: std::path::PathBuf) {
        *self.exec_override.lock().expect("exec_override poisoned") = Some(p);
    }

    /// 当前二进制路径（安装目标）：测试注入优先，缺省 `current_exe()` 推导。
    /// current_exe 失败 → Err（防呆：解析不出 exec 路径绝不继续安装）。
    fn current_binary_path(&self) -> Result<std::path::PathBuf, String> {
        if let Some(p) = self
            .exec_override
            .lock()
            .expect("exec_override poisoned")
            .clone()
        {
            return Ok(p);
        }
        std::env::current_exe().map_err(|e| format!("解析当前二进制路径失败（current_exe）: {e}"))
    }

    /// 当前通道快照（测试用）。
    #[must_use]
    pub fn channel_snapshot(&self) -> String {
        self.state.lock().expect("state poisoned").channel.clone()
    }

    /// 任务列表快照（测试用）。
    #[must_use]
    pub fn tasks_snapshot(&self) -> Vec<UpdateTask> {
        self.state.lock().expect("state poisoned").tasks.clone()
    }

    /// 持久化当前状态（路径为 None 时空操作；失败打日志不阻塞请求）。
    fn persist(&self) {
        let Some(path) = &self.state_path else { return };
        let st = self.state.lock().expect("state poisoned").clone();
        if let Err(e) = persist_state_to(path, &st) {
            eprintln!("[update] 状态落盘失败 {path}: {e}");
        }
    }

    /// 读 NexHub 裸仓库 tag 列表（tag 名 + 打 tag 时间）。
    ///
    /// 一条 `git for-each-ref refs/tags` 子进程（tag 与 creatordate 一次取回）。
    /// 仓库不存在 / git 不可用 / 输出异常 → 空 Vec（调用方按"无可用更新"降级）。
    async fn list_repo_tags(&self) -> Vec<(String, Option<String>)> {
        let out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(&self.repo_path)
            .args(["for-each-ref", "refs/tags", "--format"])
            .arg("%(refname:short)%09%(creatordate:iso-strict)")
            .output()
            .await;
        let Ok(o) = out else { return Vec::new() };
        if !o.status.success() {
            return Vec::new();
        }
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(2, '\t');
                let tag = parts.next()?.trim().to_string();
                if tag.is_empty() {
                    return None;
                }
                let at = parts
                    .next()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                Some((tag, at))
            })
            .collect()
    }

    /// 远端更新源 tag 列表：`git ls-remote --tags --refs <url>` 纯网络查询
    /// （无需本地克隆；解析复用 [`parse_ls_remote_output`]）。
    ///
    /// git 不可用 / URL 不可达 / 认证失败 → 空 Vec（两级源解析链的第二级，
    /// 失败即整体降级）。URL 不可达时 ls-remote 会挂到 TCP 超时，此处封顶
    /// [`LS_REMOTE_TIMEOUT_SECS`] 保证 check 请求不长时间悬挂（超时按失败处理）。
    async fn list_remote_tags(&self, url: &str) -> Vec<(String, Option<String>)> {
        let run = tokio::process::Command::new("git")
            .args(["ls-remote", "--tags", "--refs", url])
            .output();
        let Ok(out) =
            tokio::time::timeout(std::time::Duration::from_secs(LS_REMOTE_TIMEOUT_SECS), run).await
        else {
            eprintln!("[update] ls-remote 超时（>{LS_REMOTE_TIMEOUT_SECS}s）: {url}");
            return Vec::new();
        };
        let Ok(o) = out else { return Vec::new() };
        if !o.status.success() {
            eprintln!(
                "[update] ls-remote 失败（{}）: {url}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            return Vec::new();
        }
        parse_ls_remote_output(&String::from_utf8_lossy(&o.stdout))
    }

    /// 两级源解析链 → tag 列表 + 实际采用的源描述。
    ///
    /// 1. 本地裸仓库 `for-each-ref`（有本地副本的节点走本级，现状不变）；
    /// 2. 本地无 tag 且配置了远端 URL → `ls-remote --tags`（install.sh 装的
    ///    新节点无 `/tank`，走本级读安装源节点的 NexHub）；
    /// 3. 均无 tag → `(空, local路径, none)` 降级。
    ///
    /// 返回 (tags, repo 描述, repo_mode)；mode ∈ {local, remote, none}。
    async fn resolve_tags(&self) -> (Vec<(String, Option<String>)>, String, String) {
        let local = self.list_repo_tags().await;
        if !local.is_empty() {
            return (local, self.repo_path.clone(), "local".into());
        }
        if let Some(url) = self.repo_url.clone() {
            let remote = self.list_remote_tags(&url).await;
            if !remote.is_empty() {
                return (remote, url, "remote".into());
            }
        }
        (Vec::new(), self.repo_path.clone(), "none".into())
    }

    /// `GET /status` 视图快照。
    fn status_view(&self) -> UpdateStatusView {
        let st = self.state.lock().expect("state poisoned");
        let slots = self.slots.lock().expect("slots poisoned");
        let active = slots
            .active_slot()
            .map(slot_label)
            .unwrap_or_else(|| "a".to_string());
        let writable = slots
            .writable_slot()
            .map(slot_label)
            .unwrap_or_else(|_| "b".to_string());
        UpdateStatusView {
            current_version: self.current_version.clone(),
            channel: st.channel.clone(),
            slot_a: serde_json::to_value(&slots.a).unwrap_or(serde_json::Value::Null),
            slot_b: serde_json::to_value(&slots.b).unwrap_or(serde_json::Value::Null),
            active_slot: active,
            writable_slot: writable,
            last_check: st.last_check.clone(),
            pending_updates: st.available.clone(),
            state_path: self.state_path.clone(),
        }
    }

    /// `POST /apply` 建任务：校验版本合法且新于当前；快照工件路径 + sha256
    /// 进任务（避免任务进行中工件被重新登记导致校验基准漂移）。
    fn create_task(
        &self,
        version: &str,
        tag: Option<String>,
        channel: &str,
        artifact: &UpdateArtifact,
    ) -> Option<UpdateTask> {
        let target = Version::parse(version).ok()?;
        let current = Version::parse(&self.current_version).ok()?;
        if target <= current {
            return None; // 不可降级 / 不可平级重装
        }
        let id = {
            let mut c = self.counter.lock().expect("counter poisoned");
            *c += 1;
            format!("update-{}", *c)
        };
        let now = now_iso();
        // A/B 双槽语义：写入目标始终是"另一槽"（当前 active=A → 目标 B）。
        let slot_target = {
            let slots = self.slots.lock().expect("slots poisoned");
            slots
                .writable_slot()
                .map(slot_label)
                .unwrap_or_else(|_| "b".into())
        };
        Some(UpdateTask {
            id,
            version: target.as_string(),
            tag,
            channel: channel.to_string(),
            status: "pending".into(),
            slot_target,
            progress: 0,
            created_at: now.clone(),
            updated_at: now,
            error: None,
            note: None,
            artifact_path: Some(artifact.path.clone()),
            artifact_sha256: Some(artifact.sha256.clone()),
        })
    }

    /// 任务状态机推进一步（GET /tasks/:id 轮询调用；终态不动）。
    ///
    /// 真实安装管线：`verifying` 做工件 sha256 重算比对 + ELF 魔数复核；
    /// `writing` 做 staged 拷贝 + 备份 + rename 安装（成功后按生产配置
    /// 调度 systemctl 自重启）。任一步失败 → `failed` 带原因（不再前进）。
    fn step_task(&self, t: &mut UpdateTask) {
        match t.status.as_str() {
            "pending" => {
                // 工件已在登记时经 Files API 落到本机，无下载阶段。
                t.status = "verifying".into();
                t.progress = 40;
            }
            "verifying" => self.verify_artifact(t),
            "writing" => self.install_artifact(t),
            "reboot_pending" => {
                t.status = "done".into();
                t.progress = 100;
            }
            _ => {} // done / failed / 未知：终态不动
        }
        t.updated_at = now_iso();
    }

    /// verifying 阶段：工件真实复核（sha256 重算比对 + ELF 魔数复核）。
    ///
    /// 防登记后文件被替换/截断（Files API 落盘与 apply 之间的窗口）。
    /// 通过 → `writing`；任一不过 → `failed` 带原因。
    fn verify_artifact(&self, t: &mut UpdateTask) {
        let fail = |t: &mut UpdateTask, why: String| {
            t.status = "failed".into();
            t.error = Some(why);
        };
        let Some(path) = t.artifact_path.clone() else {
            fail(t, "任务缺少工件路径（旧版任务？请重新 apply）".into());
            return;
        };
        let Some(want) = t.artifact_sha256.clone() else {
            fail(t, "任务缺少工件 sha256（旧版任务？请重新 apply）".into());
            return;
        };
        let p = std::path::Path::new(&path);
        match elf_magic_ok(p) {
            Ok(true) => {}
            Ok(false) => {
                fail(
                    t,
                    format!("工件 {path:?} 非 ELF（魔数复核不通过），拒绝安装"),
                );
                return;
            }
            Err(e) => {
                fail(t, e);
                return;
            }
        }
        match sha256_file(p) {
            Ok(got) if got == want => {
                t.status = "writing".into();
                t.progress = 70;
            }
            Ok(got) => fail(
                t,
                format!("工件 sha256 不匹配：登记 {want}，实测 {got}（文件已被改动）"),
            ),
            Err(e) => fail(t, e),
        }
    }

    /// writing 阶段：真实安装（staged 拷贝 → 备份 → rename 覆盖当前二进制）。
    ///
    /// 防呆（红线）：exec 路径必须解析成功才继续（current_exe 失败 → 任务
    /// failed 带原因）；绝不安装非 ELF（verifying 已复核，此处不再重复但
    /// staged 内容即复核通过的工件字节）。Linux 对运行中二进制 rename-over
    /// 合法——ETXTBSY 只挡 open-for-write，rename(2) 换目录项不受影响，
    /// 运行中的旧 inode 继续有效，新 inode 就位后下次 exec 即新版本。
    /// 成功 → `reboot_pending` + note；生产配置下再调度 systemctl 自重启。
    fn install_artifact(&self, t: &mut UpdateTask) {
        let fail = |t: &mut UpdateTask, why: String| {
            t.status = "failed".into();
            t.error = Some(why);
        };
        let Some(artifact_path) = t.artifact_path.clone() else {
            fail(t, "任务缺少工件路径（旧版任务？请重新 apply）".into());
            return;
        };
        // 1. exec 路径解析（防呆：失败绝不盲装）。
        let exe = match self.current_binary_path() {
            Ok(p) => p,
            Err(e) => {
                fail(t, format!("安装中止：{e}"));
                return;
            }
        };
        let Some(exec_dir) = exe.parent().map(std::path::Path::to_path_buf) else {
            fail(
                t,
                format!("安装中止：无法从 {:?} 推导 exec 目录", exe.display()),
            );
            return;
        };
        let staged = exec_dir.join(STAGED_NAME);
        // 2. 工件拷贝到 staged + chmod 755（先落稳再切换，避免半写状态直接覆盖）。
        if let Err(e) = std::fs::copy(&artifact_path, &staged) {
            fail(
                t,
                format!(
                    "拷贝工件到 staged 失败 {:?} → {:?}: {e}",
                    artifact_path,
                    staged.display()
                ),
            );
            return;
        }
        if let Err(e) = set_exec_perm(&staged) {
            fail(t, e);
            return;
        }
        // 3. 备份当前二进制（copy 保留在运行的 inode 不动；保留最近 3 个）。
        let ts = chrono::Local::now().format("%Y%m%dT%H%M%S");
        let backup = exec_dir.join(format!("{BACKUP_PREFIX}{ts}"));
        if let Err(e) = std::fs::copy(&exe, &backup) {
            fail(t, format!("备份当前二进制失败 {:?}: {e}", exe.display()));
            return;
        }
        prune_backups(&exec_dir, BACKUP_KEEP);
        // 4. rename staged → 当前二进制路径（原子切换）。
        if let Err(e) = std::fs::rename(&staged, &exe) {
            fail(
                t,
                format!(
                    "切换二进制失败 {:?} → {:?}: {e}",
                    staged.display(),
                    exe.display()
                ),
            );
            return;
        }
        // 5. 置 reboot_pending + note；生产配置下调度自重启（测试恒跳过）。
        t.status = "reboot_pending".into();
        t.progress = 90;
        t.note = Some(RESTART_NOTE.to_string());
        if self.self_restart {
            spawn_self_restart();
        } else {
            // 红线：测试路径绝不 spawn systemctl（绝不重启开发机服务）。
            eprintln!("[update] 测试模式：跳过自重启调度");
        }
    }

    /// 工件登记：校验（semver + 绝对路径 + 存在 + 可读 + ≥1MB + ELF 魔数）
    /// 后算 sha256 入库（重复 version 覆盖）。失败返回错误消息（400）。
    fn register_artifact(&self, version: &str, path: &str) -> Result<UpdateArtifact, String> {
        let target = Version::parse(version).map_err(|_| format!("版本 {version:?} 非 semver"))?;
        let norm = target.as_string();
        let p = std::path::Path::new(path);
        if !p.is_absolute() {
            return Err(format!(
                "工件路径须为本机绝对路径（Files API 上传产物）: {path:?}"
            ));
        }
        let meta = std::fs::metadata(p)
            .map_err(|_| format!("工件不存在: {path}（请先经 Files API 上传到本机）"))?;
        if !meta.is_file() {
            return Err(format!("工件路径不是普通文件: {path}"));
        }
        if meta.len() < MIN_ARTIFACT_BYTES {
            return Err(format!(
                "工件体积 {} 字节低于下限 {MIN_ARTIFACT_BYTES}（疑似残留/半传文件）",
                meta.len()
            ));
        }
        if !elf_magic_ok(p)? {
            return Err(format!("工件非 ELF（头 4 字节魔数校验不通过）: {path}"));
        }
        let sha256 = sha256_file(p)?;
        Ok(UpdateArtifact {
            version: norm,
            path: path.to_string(),
            size: meta.len(),
            sha256,
            registered_at: now_iso(),
        })
    }

    /// 登记工件并落库（`POST /artifact` 与 prepare-distributable 的**共享
    /// 登记入口**，2026-09-03 抽出）：校验 + sha256（[`Self::register_artifact`]
    /// 全套）→ 入 `state.artifacts`（重复 version 原位覆盖 → 天然幂等）→
    /// 持久化。provisioning 的 prepare-distributable 成功后以共享实例调用
    /// 本方法自动登记同版本工件（发版流程单动作喂饱 dist + 页内 apply
    /// 两条更新通道，见 docs/UPDATE_APP.md §1a）。
    pub fn register_artifact_and_persist(
        &self,
        version: &str,
        path: &str,
    ) -> Result<UpdateArtifact, String> {
        let artifact = self.register_artifact(version, path)?;
        {
            let mut st = self.state.lock().expect("state poisoned");
            // 重复 version 覆盖（原位替换，保留列表位置）。
            match st
                .artifacts
                .iter()
                .position(|a| a.version == artifact.version)
            {
                Some(i) => st.artifacts[i] = artifact.clone(),
                None => st.artifacts.push(artifact.clone()),
            }
        }
        self.persist();
        Ok(artifact)
    }
}

impl Default for UpdateRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

/// 当前系统版本：env `NEXOS_VERSION` 优先（非空），缺省取 os-api 包版本
/// （`CARGO_PKG_VERSION` = 工作区统一 `0.1.0`，与 NexHub 发版 tag `v0.1.0`
/// 对应；os-api 无独立版本号，故以此常量为系统版本口径）。
///
/// `pub(crate)`：terminal.rs 的 node-snapshot 聚合复用（版本口径与
/// /update/status 的 current_version 同源）。
pub(crate) fn current_version_from_env() -> String {
    std::env::var("NEXOS_VERSION")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

/// UpdateSlot → 小写标签（"a"/"b"）。
fn slot_label(s: UpdateSlot) -> String {
    match s {
        UpdateSlot::A => "a".into(),
        UpdateSlot::B => "b".into(),
    }
}

#[async_trait]
impl RouteHandler for UpdateRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            spec(HttpMethod::Get, "/api/v1/update/status", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/update/channels", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/update/channel",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/update/check",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/update/artifact",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/update/artifacts", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/update/apply",
                true,
                vec!["admin".into()],
            ),
            spec(HttpMethod::Get, "/api/v1/update/tasks", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/update/tasks/:id", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/update/history", false, vec![]),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // —— GET /api/v1/update/status —— 当前版本/通道/槽位/上次检查
            (HttpMethod::Get, ["api", "v1", "update", "status"]) => {
                Ok(ok_json(to_value(&self.status_view())?))
            }

            // —— GET /api/v1/update/channels —— 通道目录 + 当前选中
            (HttpMethod::Get, ["api", "v1", "update", "channels"]) => {
                let current = self.state.lock().expect("state poisoned").channel.clone();
                Ok(ok_json(serde_json::json!({
                    "current": current,
                    "channels": to_value(&channel_catalog())?,
                })))
            }

            // —— POST /api/v1/update/channel —— 切换通道（持久化）
            (HttpMethod::Post, ["api", "v1", "update", "channel"]) => {
                let body: SetChannelBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析切换通道请求体失败: {e}"))
                })?;
                let ch = body.channel.trim().to_lowercase();
                if !ALL_CHANNELS.contains(&ch.as_str()) {
                    return Ok(error_response(
                        400,
                        &format!("非法通道 {ch:?}（合法值：{ALL_CHANNELS:?}）"),
                    ));
                }
                {
                    let mut st = self.state.lock().expect("state poisoned");
                    st.channel = ch.clone();
                }
                self.persist();
                Ok(ok_json(serde_json::json!({"channel": ch})))
            }

            // —— POST /api/v1/update/check —— 两级源解析链读 tag → 通道过滤 → semver 比较
            (HttpMethod::Post, ["api", "v1", "update", "check"]) => {
                let channel = self.state.lock().expect("state poisoned").channel.clone();
                let (tags, repo, repo_mode) = self.resolve_tags().await;
                let repo_reachable = !tags.is_empty();
                let available = Version::parse(&self.current_version)
                    .map(|cur| filter_available(&tags, &cur, &channel))
                    .unwrap_or_default();
                let checked_at = now_iso();
                {
                    let mut st = self.state.lock().expect("state poisoned");
                    st.last_check = Some(checked_at.clone());
                    st.available = available.clone();
                }
                self.persist();
                Ok(ok_json(to_value(&CheckResult {
                    current_version: self.current_version.clone(),
                    channel,
                    available,
                    checked_at,
                    repo,
                    repo_mode,
                    repo_url: self.repo_url.clone(),
                    repo_reachable,
                })?))
            }

            // —— POST /api/v1/update/artifact —— 登记工件（Files API 上传产物）
            (HttpMethod::Post, ["api", "v1", "update", "artifact"]) => {
                let body: RegisterArtifactBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析登记工件请求体失败: {e}"))
                })?;
                // 校验 + 入库 + 持久化（与 prepare-distributable 的自动登记共用
                // 同一入口，两条登记路径语义恒一致）。
                let artifact = match self
                    .register_artifact_and_persist(body.version.trim(), body.path.trim())
                {
                    Ok(a) => a,
                    Err(msg) => return Ok(error_response(400, &msg)),
                };
                let resp_body = to_value(&artifact)?;
                Ok(ApiResponse {
                    status: 201,
                    body: resp_body,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/update/artifacts —— 已登记工件列表
            (HttpMethod::Get, ["api", "v1", "update", "artifacts"]) => {
                let artifacts = self.state.lock().expect("state poisoned").artifacts.clone();
                Ok(ok_json(to_value(&artifacts)?))
            }

            // —— POST /api/v1/update/apply —— 建更新任务（真实安装管线，见模块文档）
            (HttpMethod::Post, ["api", "v1", "update", "apply"]) => {
                let body: ApplyBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析应用更新请求体失败: {e}"))
                })?;
                let version = body.version.trim().to_string();
                if version.is_empty() {
                    return Ok(error_response(400, "version 不可为空"));
                }
                let (channel, tag, artifact) = {
                    let st = self.state.lock().expect("state poisoned");
                    // 从上次 check 结果反查 tag（找不到 = 未检查过/不在清单，仍允许——manual 语义）
                    let tag = st
                        .available
                        .iter()
                        .find(|u| u.version == version || u.tag == version)
                        .map(|u| u.tag.clone());
                    // 工件前置：apply 只装已登记工件（Files API 闭环的第二环）。
                    let artifact = Version::parse(&version).ok().and_then(|v| {
                        let n = v.as_string();
                        st.artifacts.iter().find(|a| a.version == n).cloned()
                    });
                    (st.channel.clone(), tag, artifact)
                };
                // 前置一：版本须 semver 且严格新于当前（把降级/非法的 400 与
                // "未登记工件"的指引分开报）。
                let version_ok = matches!(
                    (
                        Version::parse(&version),
                        Version::parse(&self.current_version)
                    ),
                    (Ok(v), Ok(cur)) if v > cur
                );
                if !version_ok {
                    return Ok(error_response(
                        400,
                        &format!(
                            "版本 {version} 非法或不新于当前 {}（不支持降级）",
                            self.current_version
                        ),
                    ));
                }
                // 前置二：无已登记工件 → 400 指引先走 Files API + artifact 登记。
                let Some(artifact) = artifact else {
                    return Ok(error_response(
                        400,
                        &format!(
                            "版本 {version} 尚未登记更新工件：先 POST /api/v1/update/artifact \
                             {{\"version\": \"{version}\", \"path\": \"<本机绝对路径>\"}}（工件经 \
                             Files API 上传到本机后登记）"
                        ),
                    ));
                };
                let Some(task) = self.create_task(&version, tag, &channel, &artifact) else {
                    return Ok(error_response(
                        400,
                        &format!(
                            "版本 {version} 非法或不新于当前 {}（不支持降级）",
                            self.current_version
                        ),
                    ));
                };
                let resp_body = to_value(&task)?;
                {
                    let mut st = self.state.lock().expect("state poisoned");
                    st.tasks.push(task);
                }
                self.persist();
                Ok(ApiResponse {
                    status: 201,
                    body: resp_body,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/update/tasks —— 任务列表（新在前）
            (HttpMethod::Get, ["api", "v1", "update", "tasks"]) => {
                let mut tasks = self.state.lock().expect("state poisoned").tasks.clone();
                tasks.reverse();
                Ok(ok_json(to_value(&tasks)?))
            }

            // —— GET /api/v1/update/tasks/:id —— 详情（轮询即推进一步）
            (HttpMethod::Get, ["api", "v1", "update", "tasks", id]) => {
                let mut st = self.state.lock().expect("state poisoned");
                let Some(t) = st.tasks.iter_mut().find(|t| t.id == *id) else {
                    return Ok(error_response(404, &format!("更新任务不存在: {id}")));
                };
                self.step_task(t);
                let snapshot = t.clone();
                drop(st);
                self.persist();
                Ok(ok_json(to_value(&snapshot)?))
            }

            // —— GET /api/v1/update/history —— 已应用历史（done / reboot_pending）
            (HttpMethod::Get, ["api", "v1", "update", "history"]) => {
                let mut hist: Vec<UpdateTask> = self
                    .state
                    .lock()
                    .expect("state poisoned")
                    .tasks
                    .iter()
                    .filter(|t| t.status == "done" || t.status == "reboot_pending")
                    .cloned()
                    .collect();
                hist.reverse();
                Ok(ok_json(to_value(&hist)?))
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "update: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// 请求体
// ----------------------------------------------------------------------------

/// `POST /channel` 请求体。
#[derive(Debug, Deserialize)]
struct SetChannelBody {
    channel: String,
}

/// `POST /apply` 请求体。
#[derive(Debug, Deserialize)]
struct ApplyBody {
    version: String,
}

/// `POST /artifact` 请求体（Files API 上传产物登记）。
#[derive(Debug, Deserialize)]
struct RegisterArtifactBody {
    version: String,
    path: String,
}

// ----------------------------------------------------------------------------
// 内部辅助（与 provisioning.rs 同款）
// ----------------------------------------------------------------------------

/// 构造一条 RouteSpec（component 固定 `update`）。
fn spec(
    method: HttpMethod,
    path: &str,
    requires_auth: bool,
    required_roles: Vec<String>,
) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: "update".to_string(),
        requires_auth,
        required_roles,
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

/// 从请求路径剥离 `?query` 后的纯 path 段。
fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

/// 当前 ISO 时间戳（本地时区）。
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
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

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

    // —— git fixture：临时 init 裸仓库打 tag ——
    // work 仓（空提交 + 打 tag）→ clone --bare → 返回裸仓库路径。
    // TempDirGuard drop 时清理整个临时目录（workspace 未注册 tempfile，自管）。

    static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

    struct TempDirGuard(PathBuf);
    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn run_git(dir: Option<&str>, args: &[&str]) {
        let mut cmd = std::process::Command::new("git");
        if let Some(d) = dir {
            cmd.current_dir(d);
        }
        cmd.args([
            "-c",
            "user.email=update-test@nexos.local",
            "-c",
            "user.name=update-test",
        ])
        .args(args);
        let out = cmd.output().expect("git 子进程应可执行");
        assert!(
            out.status.success(),
            "git {args:?} 失败: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// 造一个打了指定 tag 的裸仓库 fixture；返回 (裸仓库路径, 清理 guard)。
    fn git_fixture(tags: &[&str]) -> (String, TempDirGuard) {
        let dir = std::env::temp_dir().join(format!(
            "nexos-update-test-{}-{}",
            std::process::id(),
            FIXTURE_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let guard = TempDirGuard(dir.clone());
        let work = dir.join("work");
        let bare = dir.join("nexos.git");
        std::fs::create_dir_all(&work).unwrap();
        run_git(Some(work.to_str().unwrap()), &["init", "-q", "."]);
        run_git(
            Some(work.to_str().unwrap()),
            &["commit", "--allow-empty", "-q", "-m", "init"],
        );
        for t in tags {
            run_git(Some(work.to_str().unwrap()), &["tag", t]);
        }
        run_git(
            Some(dir.to_str().unwrap()),
            &[
                "clone",
                "-q",
                "--bare",
                work.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );
        (bare.to_string_lossy().into_owned(), guard)
    }

    // ============ 1. 通道切换持久化往返 ============

    #[tokio::test]
    async fn channel_switch_persists_and_survives_reopen() {
        let state = std::env::temp_dir().join(format!(
            "nexos-update-state-{}-{}.json",
            std::process::id(),
            FIXTURE_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let state_str = state.to_string_lossy().into_owned();
        let _cleanup = TempDirGuard(state.clone());
        {
            let h = UpdateRouteHandler::with_config(
                Some(state_str.clone()),
                "/nonexistent/nexos.git",
                "0.1.0",
            );
            // 初始 stable
            assert_eq!(h.channel_snapshot(), "stable");
            // 切到 beta
            let resp = h
                .handle(post_req(
                    "/api/v1/update/channel",
                    serde_json::json!({"channel": "beta"}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 200, "body: {resp:?}");
            assert_eq!(resp.body["channel"], "beta");
            assert_eq!(h.channel_snapshot(), "beta");
        }
        // 重启重建 handler → 通道仍是 beta（JSON 读回）
        let h2 =
            UpdateRouteHandler::with_config(Some(state_str), "/nonexistent/nexos.git", "0.1.0");
        assert_eq!(h2.channel_snapshot(), "beta", "通道应从 JSON 读回");
    }

    // ============ 2. 非法通道 400 ============

    #[tokio::test]
    async fn channel_switch_rejects_invalid() {
        let h = UpdateRouteHandler::with_config(None, "/nonexistent/nexos.git", "0.1.0");
        for bad in ["canary", "", "STABLE-x"] {
            let resp = h
                .handle(post_req(
                    "/api/v1/update/channel",
                    serde_json::json!({"channel": bad}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 400, "通道 {bad:?} 应 400");
            assert!(resp.body["error"].as_str().unwrap().contains("非法通道"));
        }
        // 大写归一化合法（NIGHTLY → nightly）
        let resp = h
            .handle(post_req(
                "/api/v1/update/channel",
                serde_json::json!({"channel": "NIGHTLY"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["channel"], "nightly");
    }

    // ============ 3. check 通道过滤（stable 排除 -beta）============

    #[tokio::test]
    async fn check_stable_channel_excludes_prereleases() {
        let (bare, _g) = git_fixture(&[
            "v0.0.9",      // 旧版（不可用）
            "v0.1.0",      // 与当前相同（不可用）
            "v0.2.0",      // 新正式版（可用）
            "v0.3.0-beta", // beta 预发布（stable 排除）
            "v0.4.0-rc1",  // 其它预发布（stable 排除）
            "windows-m1",  // 非 semver tag（忽略）
        ]);
        let h = UpdateRouteHandler::with_config(None, bare, "0.1.0");
        let resp = h
            .handle(post_req("/api/v1/update/check", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "body: {resp:?}");
        assert_eq!(resp.body["repo_reachable"], true);
        assert_eq!(resp.body["current_version"], "0.1.0");
        let avail = resp.body["available"].as_array().unwrap();
        assert_eq!(avail.len(), 1, "stable 只见 v0.2.0: {avail:?}");
        assert_eq!(avail[0]["tag"], "v0.2.0");
        assert_eq!(avail[0]["version"], "0.2.0");
        assert_eq!(avail[0]["channel"], "stable");
        assert!(
            avail[0]["created_at"].as_str().is_some(),
            "tag 时间应从 git creatordate 解析"
        );
    }

    // ============ 4. check 通道过滤（beta 只收 -beta）============

    #[tokio::test]
    async fn check_beta_channel_only_beta_tags() {
        let (bare, _g) = git_fixture(&["v0.2.0", "v0.3.0-beta", "v0.4.0-rc1"]);
        let h = UpdateRouteHandler::with_config(None, bare, "0.1.0");
        h.handle(post_req(
            "/api/v1/update/channel",
            serde_json::json!({"channel": "beta"}),
        ))
        .await
        .unwrap();
        let resp = h
            .handle(post_req("/api/v1/update/check", serde_json::json!({})))
            .await
            .unwrap();
        let avail = resp.body["available"].as_array().unwrap();
        assert_eq!(avail.len(), 1, "beta 只收 *-beta*: {avail:?}");
        assert_eq!(avail[0]["tag"], "v0.3.0-beta");
        assert_eq!(avail[0]["channel"], "beta");
    }

    // ============ 5. check 通道过滤（nightly / manual 全收）============

    #[tokio::test]
    async fn check_nightly_channel_takes_all() {
        let (bare, _g) = git_fixture(&["v0.2.0", "v0.3.0-beta", "v0.4.0-rc1"]);
        let h = UpdateRouteHandler::with_config(None, bare, "0.1.0");
        h.handle(post_req(
            "/api/v1/update/channel",
            serde_json::json!({"channel": "nightly"}),
        ))
        .await
        .unwrap();
        let resp = h
            .handle(post_req("/api/v1/update/check", serde_json::json!({})))
            .await
            .unwrap();
        let avail = resp.body["available"].as_array().unwrap();
        assert_eq!(avail.len(), 3, "nightly 全收: {avail:?}");
        // 按版本降序：0.4.0-rc1 > 0.3.0-beta > 0.2.0
        assert_eq!(avail[0]["version"], "0.4.0-rc1");
        assert_eq!(avail[1]["version"], "0.3.0-beta");
        assert_eq!(avail[2]["version"], "0.2.0");
        // manual 同样全收
        h.handle(post_req(
            "/api/v1/update/channel",
            serde_json::json!({"channel": "manual"}),
        ))
        .await
        .unwrap();
        let resp = h
            .handle(post_req("/api/v1/update/check", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.body["available"].as_array().unwrap().len(), 3);
    }

    // ============ 6. 仓库缺失降级：空清单不报错 ============

    #[tokio::test]
    async fn check_missing_repo_degrades_to_empty() {
        let h = UpdateRouteHandler::with_config(None, "/nonexistent/nexos.git", "0.1.0");
        let resp = h
            .handle(post_req("/api/v1/update/check", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "缺仓库不报错: {resp:?}");
        assert_eq!(resp.body["repo_reachable"], false);
        assert_eq!(resp.body["available"].as_array().unwrap().len(), 0);
        // last_check 仍被记录（status 可见）
        let st = h.handle(get_req("/api/v1/update/status")).await.unwrap();
        assert!(st.body["last_check"].as_str().is_some());
        assert_eq!(st.body["pending_updates"].as_array().unwrap().len(), 0);
    }

    // ============ 6a. ls-remote 输出解析纯函数 ============

    #[test]
    fn ls_remote_output_parse_shape() {
        // 典型输出：annotated tag 带 peeled `^{}` 行（--refs 缺席/旧 git 时防重复）
        let raw = "0123456789abcdef0123456789abcdef01234567\trefs/tags/v0.1.0\n\
                   fedcba9876543210fedcba9876543210fedcba98\trefs/tags/v0.2.0\n\
                   1111111111111111111111111111111111111111\trefs/tags/v0.2.0^{}\n\
                   2222222222222222222222222222222222222222\trefs/tags/windows-m1\n";
        let tags = parse_ls_remote_output(raw);
        assert_eq!(
            tags.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
            vec!["v0.1.0", "v0.2.0", "windows-m1"],
            "peeled ^{{}} 行应被过滤: {tags:?}"
        );
        // ls-remote 不携带时间——created_at 恒 None（filter_available 通道口径复用）
        assert!(tags.iter().all(|(_, at)| at.is_none()));
        // 通道语义复用：nightly 全收 + 降序；stable 排除预发布（与本地 tag 同一过滤器）
        let cur = Version::parse("0.1.0").unwrap();
        let nightly = filter_available(&tags, &cur, "nightly");
        assert_eq!(
            nightly
                .iter()
                .map(|u| u.version.as_str())
                .collect::<Vec<_>>(),
            vec!["0.2.0"]
        );
        assert_eq!(nightly[0].tag, "v0.2.0");
        assert!(nightly[0].created_at.is_none());
        // 异常行防御：空行 / 缺 tab / 缺 refs/tags/ 前缀 / 空 tag 名均忽略
        let junk = "\n\nonly-hash-no-tab\nsha\trefs/heads/main\nsha\trefs/tags/\n";
        assert!(parse_ls_remote_output(junk).is_empty(), "异常行应全忽略");
        assert!(parse_ls_remote_output("").is_empty());
    }

    // ============ 6b. 源解析链分支二：本地缺失 → 远端 ls-remote ============

    // git_fixture 的裸仓库路径可被 ls-remote 直接查询（git 对本地路径与
    // http(s) URL 同协议处理），无需起 TCP mock 即走真实 ls-remote 子进程。
    #[tokio::test]
    async fn check_remote_fallback_when_local_missing() {
        let (bare, _g) = git_fixture(&["v0.1.0", "v0.2.0", "v0.3.0-beta"]);
        let h = UpdateRouteHandler::with_config_and_remote(
            None,
            "/nonexistent/nexos.git",
            Some(bare.clone()),
            "0.1.0",
        );
        let resp = h
            .handle(post_req("/api/v1/update/check", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "body: {resp:?}");
        assert_eq!(
            resp.body["repo_reachable"], true,
            "远端模式应可达: {resp:?}"
        );
        assert_eq!(resp.body["repo_mode"], "remote");
        assert_eq!(resp.body["repo"], bare, "repo 应为实际采用的远端 URL");
        assert_eq!(resp.body["repo_url"], bare.as_str());
        // stable 过滤：排除 -beta（通道语义与本地 tag 同一口径）
        let avail = resp.body["available"].as_array().unwrap();
        assert_eq!(avail.len(), 1, "stable 只见 v0.2.0: {avail:?}");
        assert_eq!(avail[0]["tag"], "v0.2.0");
        // ls-remote 拿不到 creatordate → created_at 为 null（不猜时间）
        assert!(avail[0]["created_at"].is_null());
    }

    // ============ 6c. 源解析链分支一：本地有 tag 优先（远端已配置也不抢） ============

    #[tokio::test]
    async fn check_local_repo_preferred_over_remote() {
        let (local, _g1) = git_fixture(&["v0.2.0"]);
        let (remote, _g2) = git_fixture(&["v0.9.0"]);
        let h =
            UpdateRouteHandler::with_config_and_remote(None, local.clone(), Some(remote), "0.1.0");
        let resp = h
            .handle(post_req("/api/v1/update/check", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.body["repo_reachable"], true);
        assert_eq!(
            resp.body["repo_mode"], "local",
            "本地有 tag 时本地优先: {resp:?}"
        );
        assert_eq!(resp.body["repo"], local);
        let avail = resp.body["available"].as_array().unwrap();
        assert_eq!(avail.len(), 1);
        assert_eq!(avail[0]["tag"], "v0.2.0", "应读本地 tag 而非远端 v0.9.0");
        assert!(
            avail[0]["created_at"].as_str().is_some(),
            "本地模式保留 creatordate"
        );
    }

    // ============ 6d. 源解析链分支三：本地与远端均不可达 → 降级空清单 ============

    #[tokio::test]
    async fn check_both_sources_unreachable_degrades_with_three_state() {
        let h = UpdateRouteHandler::with_config_and_remote(
            None,
            "/nonexistent/nexos.git",
            Some("/nonexistent/remote/nexos.git".to_string()),
            "0.1.0",
        );
        let resp = h
            .handle(post_req("/api/v1/update/check", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "均不可达不报错: {resp:?}");
        assert_eq!(resp.body["repo_reachable"], false);
        assert_eq!(resp.body["repo_mode"], "none");
        assert_eq!(
            resp.body["repo"], "/nonexistent/nexos.git",
            "降级时 repo 保持本地路径口径（向后兼容）"
        );
        assert_eq!(
            resp.body["repo_url"].as_str(),
            Some("/nonexistent/remote/nexos.git"),
            "repo_url 回显配置值（前端三态提示用）"
        );
        assert_eq!(resp.body["available"].as_array().unwrap().len(), 0);
        // 未配置远端（with_config）时同样降级，repo_url 为 null
        let h2 = UpdateRouteHandler::with_config(None, "/nonexistent/nexos.git", "0.1.0");
        let resp = h2
            .handle(post_req("/api/v1/update/check", serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(resp.body["repo_reachable"], false);
        assert_eq!(resp.body["repo_mode"], "none");
        assert!(resp.body["repo_url"].is_null());
    }

    // ============ 7. apply 端到端真实安装（staged/备份/rename/reboot_pending）============

    /// 造一个 ≥1MB 的 ELF 工件：复制 /bin/true（真实 ELF，魔数头原样保留）
    /// 后补零填充过 1MB 登记门槛（填充不改 ELF 魔数校验结果）。
    fn make_padded_elf(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
        use std::io::Write;
        let p = dir.join(name);
        std::fs::copy("/bin/true", &p).expect("/bin/true 应存在（Linux 开发/CI 机）");
        let size = std::fs::metadata(&p).unwrap().len();
        let floor = MIN_ARTIFACT_BYTES + 4096;
        if size < floor {
            let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
            // 补零过体积门槛 + 追加文件名做盐（不同 name 的工件内容可区分）。
            f.write_all(&vec![0u8; (floor - size) as usize]).unwrap();
            f.write_all(name.as_bytes()).unwrap();
        }
        p
    }

    /// 列出目录内 `os-api.bak-*` 备份名（字典序 = 时间序）。
    fn backups_in(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with(BACKUP_PREFIX))
            .collect();
        v.sort();
        v
    }

    /// 端到端安装夹具：临时 exec 目录（伪当前二进制 + 3 个预置旧备份）+
    /// 已登记 v0.2.0 工件的 handler（exec 目标注入临时目录）。返回
    /// (清理 guard, exec 目录, 安装前二进制字节, 工件字节, handler)。
    /// 红线（RED LINE）：本夹具支撑的测试真实执行 staged 拷贝/备份/rename
    /// 安装 I/O，但 1) 安装目标注入临时目录——绝不触碰真实 os-api 或
    /// cargo test 进程二进制；2) with_config 构造 → self_restart 恒 false——
    /// 绝不 spawn systemctl 自重启（绝不重启开发机服务）。
    async fn install_fixture(
        repo: &str,
    ) -> (
        TempDirGuard,
        std::path::PathBuf,
        Vec<u8>,
        Vec<u8>,
        UpdateRouteHandler,
    ) {
        let dir = std::env::temp_dir().join(format!(
            "nexos-update-install-{}-{}",
            std::process::id(),
            FIXTURE_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let guard = TempDirGuard(dir.clone());
        // 伪「当前二进制」：可执行文件（内容任意——安装只读它做备份再 rename 覆盖）。
        let exe = dir.join("os-api");
        std::fs::copy("/bin/true", &exe).unwrap();
        set_exec_perm(&exe).expect("伪二进制应可 chmod 755");
        let original = std::fs::read(&exe).unwrap();
        // 预置 3 个旧备份（时间戳远早于现在，验证清理保留最近 3 个）。
        for ts in ["20200101T000000", "20200101T000001", "20200101T000002"] {
            std::fs::write(dir.join(format!("{BACKUP_PREFIX}{ts}")), b"stale-backup").unwrap();
        }
        // 工件：真实 ELF（/bin/true 复制）填充到 ≥1MB。
        let artifact_path = make_padded_elf(&dir, "artifact-0.2.0.bin");
        let artifact_bytes = std::fs::read(&artifact_path).unwrap();
        let h = UpdateRouteHandler::with_config(None, repo, "0.1.0");
        h.set_exec_path_for_test(exe.clone());
        let resp = h
            .handle(post_req(
                "/api/v1/update/artifact",
                serde_json::json!({
                    "version": "0.2.0",
                    "path": artifact_path.to_string_lossy(),
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "工件登记应成功: {resp:?}");
        (guard, dir, original, artifact_bytes, h)
    }

    #[tokio::test]
    async fn apply_end_to_end_real_install() {
        // git fixture：先 check 让 apply 能反查 tag（v0.2.0 → tag 反查覆盖）。
        let (bare, _g) = git_fixture(&["v0.2.0"]);
        let (_guard, dir, original, artifact_bytes, h) = install_fixture(&bare).await;
        h.handle(post_req("/api/v1/update/check", serde_json::json!({})))
            .await
            .unwrap();
        // apply 建任务（登记工件已在夹具完成）。
        let resp = h
            .handle(post_req(
                "/api/v1/update/apply",
                serde_json::json!({"version": "0.2.0"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "body: {resp:?}");
        assert_eq!(resp.body["status"], "pending");
        assert_eq!(resp.body["slot_target"], "b", "写入目标应为另一槽 B");
        assert_eq!(resp.body["tag"], "v0.2.0", "tag 从 check 结果反查");
        assert!(
            resp.body["artifact_path"]
                .as_str()
                .is_some_and(|p| p.ends_with("artifact-0.2.0.bin")),
            "任务应快照工件路径: {resp:?}"
        );
        assert!(
            resp.body["artifact_sha256"]
                .as_str()
                .is_some_and(|s| s.len() == 64),
            "任务应快照登记 sha256"
        );
        let id = resp.body["id"].as_str().unwrap().to_string();
        // 轮询推进：pending→verifying→writing→reboot_pending→done
        // （verifying 轮做 sha256+ELF 复核；writing 轮做 staged/备份/rename 安装）。
        let expect = [
            ("verifying", 40u8),
            ("writing", 70),
            ("reboot_pending", 90),
            ("done", 100),
        ];
        for (want_status, want_progress) in expect {
            let resp = h
                .handle(get_req(&format!("/api/v1/update/tasks/{id}")))
                .await
                .unwrap();
            assert_eq!(resp.status, 200);
            assert_eq!(resp.body["status"], want_status, "推进到 {want_status}");
            assert_eq!(resp.body["progress"], want_progress);
        }
        // 终态响应仍带自重启说明（writing 完成时写入）。
        let resp = h
            .handle(get_req(&format!("/api/v1/update/tasks/{id}")))
            .await
            .unwrap();
        assert_eq!(
            resp.body["note"].as_str().unwrap_or_default(),
            "已写入，服务将在数秒内自重启"
        );
        // —— 安装断言 ——
        // rename 生效：当前二进制路径上的字节 == 工件字节。
        assert_eq!(
            std::fs::read(dir.join("os-api")).unwrap(),
            artifact_bytes,
            "rename 应把工件切到当前二进制路径"
        );
        // staged 被 rename 走（不留半成品）。
        assert!(
            !dir.join(STAGED_NAME).exists(),
            "staged 应被 rename 走: {:?}",
            backups_in(&dir)
        );
        // 备份：预置 3 旧 + 新 1 = 4 → 清最老留 3；新备份内容 == 安装前二进制。
        let baks = backups_in(&dir);
        assert_eq!(baks.len(), 3, "备份保留最近 3 个: {baks:?}");
        assert!(
            !dir.join(format!("{BACKUP_PREFIX}20200101T000000")).exists(),
            "最老备份应被清理"
        );
        assert_eq!(
            std::fs::read(dir.join(baks.last().unwrap())).unwrap(),
            original,
            "新备份内容应为安装前的当前二进制"
        );
        // history 收录（done）。
        let hist = h.handle(get_req("/api/v1/update/history")).await.unwrap();
        assert_eq!(hist.body.as_array().unwrap().len(), 1);
        assert_eq!(hist.body[0]["id"], id);
    }

    // ============ 8. apply 校验：非法版本 / 降级 400 ============

    #[tokio::test]
    async fn apply_rejects_invalid_or_downgrade_version() {
        let h = UpdateRouteHandler::with_config(None, "/nonexistent/nexos.git", "0.1.0");
        for bad in ["", "not-a-version", "0.1.0", "0.0.9"] {
            let resp = h
                .handle(post_req(
                    "/api/v1/update/apply",
                    serde_json::json!({"version": bad}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 400, "版本 {bad:?} 应 400");
        }
        assert!(h.tasks_snapshot().is_empty(), "不应产生任务");
        // 不存在的任务 404
        let resp = h
            .handle(get_req("/api/v1/update/tasks/update-99"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // ============ 8b. 工件登记：形状 + 持久化 + 重复覆盖 ============

    #[tokio::test]
    async fn artifact_register_shape_persist_and_overwrite() {
        let dir = std::env::temp_dir().join(format!(
            "nexos-update-artifact-{}-{}",
            std::process::id(),
            FIXTURE_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let _guard = TempDirGuard(dir.clone());
        let state = dir.join("update-state.json");
        let art1 = make_padded_elf(&dir, "art1.bin");
        let art2 = make_padded_elf(&dir, "art2.bin");
        let h = UpdateRouteHandler::with_config(
            Some(state.to_string_lossy().into_owned()),
            "/nonexistent/nexos.git",
            "0.1.0",
        );
        // 登记形状：version/size/sha256/registered_at 齐 + 归一化（剥 v 前缀不适用，
        // semver 原样；size 为真实字节数）。
        let resp = h
            .handle(post_req(
                "/api/v1/update/artifact",
                serde_json::json!({"version": "0.2.0", "path": art1.to_string_lossy()}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "body: {resp:?}");
        assert_eq!(resp.body["version"], "0.2.0");
        assert_eq!(resp.body["path"], art1.to_string_lossy().to_string());
        assert_eq!(
            resp.body["size"].as_u64().unwrap(),
            std::fs::metadata(&art1).unwrap().len()
        );
        let sha1 = resp.body["sha256"].as_str().unwrap().to_string();
        assert_eq!(sha1.len(), 64, "sha256 应为 64 hex 字符");
        assert!(resp.body["registered_at"].as_str().is_some());
        // GET /artifacts 列表可见。
        let resp = h.handle(get_req("/api/v1/update/artifacts")).await.unwrap();
        let list = resp.body.as_array().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["version"], "0.2.0");
        assert_eq!(list[0]["sha256"], sha1.as_str());
        // 重启读回：工件随状态 JSON 持久化。
        let h2 = UpdateRouteHandler::with_config(
            Some(state.to_string_lossy().into_owned()),
            "/nonexistent/nexos.git",
            "0.1.0",
        );
        let resp = h2
            .handle(get_req("/api/v1/update/artifacts"))
            .await
            .unwrap();
        assert_eq!(
            resp.body.as_array().unwrap().len(),
            1,
            "工件应随 update-state.json 持久化"
        );
        // 重复 version 覆盖：同版本登记另一工件 → 列表仍 1 条且 sha/size 更新。
        let resp = h2
            .handle(post_req(
                "/api/v1/update/artifact",
                serde_json::json!({"version": "0.2.0", "path": art2.to_string_lossy()}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let sha2 = resp.body["sha256"].as_str().unwrap().to_string();
        assert_ne!(sha1, sha2, "两个不同工件的 sha256 应不同");
        let resp = h2
            .handle(get_req("/api/v1/update/artifacts"))
            .await
            .unwrap();
        let list = resp.body.as_array().unwrap();
        assert_eq!(list.len(), 1, "重复 version 覆盖不增条目");
        assert_eq!(list[0]["sha256"], sha2.as_str());
        assert_eq!(list[0]["path"], art2.to_string_lossy().to_string());
    }

    // ============ 8c. 工件登记：拒绝非 ELF（≥1MB 文本文件）============

    #[tokio::test]
    async fn artifact_register_rejects_non_elf() {
        let dir = std::env::temp_dir().join(format!(
            "nexos-update-nonelf-{}-{}",
            std::process::id(),
            FIXTURE_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let _guard = TempDirGuard(dir.clone());
        // 1MB+ 的文本文件：体积门槛通过，但头 4 字节不是 \x7fELF。
        let txt = dir.join("fake.bin");
        std::fs::write(
            &txt,
            "not-an-elf but big enough padding padding padding".repeat(40_000),
        )
        .unwrap();
        assert!(std::fs::metadata(&txt).unwrap().len() >= MIN_ARTIFACT_BYTES);
        let h = UpdateRouteHandler::with_config(None, "/nonexistent/nexos.git", "0.1.0");
        let resp = h
            .handle(post_req(
                "/api/v1/update/artifact",
                serde_json::json!({"version": "0.2.0", "path": txt.to_string_lossy()}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "非 ELF 应 400: {resp:?}");
        let err = resp.body["error"].as_str().unwrap();
        assert!(err.contains("ELF"), "错误应点名 ELF 魔数: {err}");
        // 登记列表不受污染。
        let resp = h.handle(get_req("/api/v1/update/artifacts")).await.unwrap();
        assert_eq!(resp.body.as_array().unwrap().len(), 0);
    }

    // ============ 8d. 工件登记：不存在路径 / 相对路径 / 非 semver 400 ============

    #[tokio::test]
    async fn artifact_register_rejects_bad_version_and_path() {
        let dir = std::env::temp_dir().join(format!(
            "nexos-update-badpath-{}-{}",
            std::process::id(),
            FIXTURE_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let _guard = TempDirGuard(dir.clone());
        let art = make_padded_elf(&dir, "good.bin");
        let h = UpdateRouteHandler::with_config(None, "/nonexistent/nexos.git", "0.1.0");
        // 不存在路径。
        let resp = h
            .handle(post_req(
                "/api/v1/update/artifact",
                serde_json::json!({
                    "version": "0.2.0",
                    "path": "/nonexistent/artifact.bin"
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "不存在路径应 400: {resp:?}");
        assert!(
            resp.body["error"].as_str().unwrap().contains("工件不存在"),
            "错误应指引文件不存在: {resp:?}"
        );
        // 相对路径（Files API 产物须为本机绝对路径）。
        let resp = h
            .handle(post_req(
                "/api/v1/update/artifact",
                serde_json::json!({"version": "0.2.0", "path": "relative/artifact.bin"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "相对路径应 400: {resp:?}");
        assert!(
            resp.body["error"].as_str().unwrap().contains("绝对路径"),
            "错误应点名绝对路径: {resp:?}"
        );
        // 非 semver 版本（文件本身合法）。
        let resp = h
            .handle(post_req(
                "/api/v1/update/artifact",
                serde_json::json!({"version": "latest", "path": art.to_string_lossy()}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "非 semver 版本应 400: {resp:?}");
        assert!(
            resp.body["error"].as_str().unwrap().contains("semver"),
            "错误应点名 semver: {resp:?}"
        );
    }

    // ============ 8e. 工件登记：拒绝 <1MB 小文件（防残留/半传）============

    #[tokio::test]
    async fn artifact_register_rejects_undersize() {
        let dir = std::env::temp_dir().join(format!(
            "nexos-update-small-{}-{}",
            std::process::id(),
            FIXTURE_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let _guard = TempDirGuard(dir.clone());
        // 真实 ELF 但体积 <1MB（/bin/true 本体 35KB）：合法魔数也过不了体积门槛。
        let small = dir.join("small.bin");
        std::fs::copy("/bin/true", &small).unwrap();
        assert!(std::fs::metadata(&small).unwrap().len() < MIN_ARTIFACT_BYTES);
        let h = UpdateRouteHandler::with_config(None, "/nonexistent/nexos.git", "0.1.0");
        let resp = h
            .handle(post_req(
                "/api/v1/update/artifact",
                serde_json::json!({"version": "0.2.0", "path": small.to_string_lossy()}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "小文件应 400: {resp:?}");
        assert!(
            resp.body["error"].as_str().unwrap().contains("低于下限"),
            "错误应点名体积下限: {resp:?}"
        );
    }

    // ============ 8f. apply 无已登记工件：400 指引先登记 ============

    #[tokio::test]
    async fn apply_without_artifact_returns_400_with_guidance() {
        let h = UpdateRouteHandler::with_config(None, "/nonexistent/nexos.git", "0.1.0");
        let resp = h
            .handle(post_req(
                "/api/v1/update/apply",
                serde_json::json!({"version": "0.2.0"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 400, "未登记工件应 400: {resp:?}");
        let err = resp.body["error"].as_str().unwrap();
        assert!(
            err.contains("/api/v1/update/artifact"),
            "错误应指引 artifact 端点: {err}"
        );
        assert!(
            err.contains("Files API"),
            "错误应指引 Files API 上传: {err}"
        );
        assert!(h.tasks_snapshot().is_empty(), "不应产生任务");
    }

    // ============ 8g. apply sha256 不匹配：failed 且绝不安装 ============

    #[tokio::test]
    async fn apply_sha256_mismatch_marks_task_failed_and_no_install() {
        let (_guard, dir, original, _artifact_bytes, h) =
            install_fixture("/nonexistent/nexos.git").await;
        // 篡改已登记工件：追加 1 字节（ELF 魔数不变、体积仍 ≥1MB，sha256 变）。
        {
            use std::io::Write;
            let art = dir.join("artifact-0.2.0.bin");
            let mut f = std::fs::OpenOptions::new().append(true).open(&art).unwrap();
            f.write_all(&[0u8]).unwrap();
        }
        let resp = h
            .handle(post_req(
                "/api/v1/update/apply",
                serde_json::json!({"version": "0.2.0"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let id = resp.body["id"].as_str().unwrap().to_string();
        // 第 1 轮 → verifying；第 2 轮复核失败 → failed。
        let resp = h
            .handle(get_req(&format!("/api/v1/update/tasks/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.body["status"], "verifying");
        let resp = h
            .handle(get_req(&format!("/api/v1/update/tasks/{id}")))
            .await
            .unwrap();
        assert_eq!(
            resp.body["status"], "failed",
            "sha256 不匹配应 failed: {resp:?}"
        );
        let err = resp.body["error"].as_str().unwrap();
        assert!(err.contains("sha256"), "错误应点名 sha256: {err}");
        assert!(
            err.contains("登记") && err.contains("实测"),
            "错误应展示登记值与实测值: {err}"
        );
        // 终态：再轮询不动。
        let resp = h
            .handle(get_req(&format!("/api/v1/update/tasks/{id}")))
            .await
            .unwrap();
        assert_eq!(resp.body["status"], "failed");
        // 安装绝不发生：当前二进制未被替换、无 staged、无新备份。
        assert_eq!(
            std::fs::read(dir.join("os-api")).unwrap(),
            original,
            "校验失败绝不 rename 覆盖"
        );
        assert!(!dir.join(STAGED_NAME).exists(), "校验失败不应产生 staged");
        assert_eq!(
            backups_in(&dir).len(),
            3,
            "校验失败不应新增备份（仅夹具预置的 3 个旧备份）"
        );
        // history 不收录 failed 任务。
        let hist = h.handle(get_req("/api/v1/update/history")).await.unwrap();
        assert_eq!(hist.body.as_array().unwrap().len(), 0);
    }

    // ============ 8h. 任务持久化：重启重建后仍在且停在原状态 ============

    #[tokio::test]
    async fn apply_task_persists_across_reopen_stops_in_place() {
        let dir = std::env::temp_dir().join(format!(
            "nexos-update-taskpersist-{}-{}",
            std::process::id(),
            FIXTURE_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let _guard = TempDirGuard(dir.clone());
        let state = dir.join("update-state.json");
        let state_str = state.to_string_lossy().into_owned();
        let art = make_padded_elf(&dir, "art.bin");
        let h = UpdateRouteHandler::with_config(
            Some(state_str.clone()),
            "/nonexistent/nexos.git",
            "0.1.0",
        );
        h.handle(post_req(
            "/api/v1/update/artifact",
            serde_json::json!({"version": "0.2.0", "path": art.to_string_lossy()}),
        ))
        .await
        .unwrap();
        let resp = h
            .handle(post_req(
                "/api/v1/update/apply",
                serde_json::json!({"version": "0.2.0"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        // 重启重建：任务与工件都从 JSON 读回，非终态任务停在 pending。
        let h2 =
            UpdateRouteHandler::with_config(Some(state_str), "/nonexistent/nexos.git", "0.1.0");
        let tasks = h2.tasks_snapshot();
        assert_eq!(tasks.len(), 1, "任务应随 update-state.json 持久化");
        assert_eq!(tasks[0].status, "pending", "非终态任务重启后停在原状态");
        assert_eq!(tasks[0].version, "0.2.0");
    }

    // ============ 9. status 形状（版本/通道/槽位）============

    #[tokio::test]
    async fn status_shape_slots_and_version() {
        let h = UpdateRouteHandler::with_config(None, "/nonexistent/nexos.git", "0.5.3");
        let resp = h.handle(get_req("/api/v1/update/status")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["current_version"], "0.5.3");
        assert_eq!(resp.body["channel"], "stable");
        // 槽位视图：A active 装当前版本，B inactive 空
        assert_eq!(resp.body["active_slot"], "a");
        assert_eq!(resp.body["writable_slot"], "b");
        assert_eq!(resp.body["slot_a"]["status"], "active");
        assert_eq!(resp.body["slot_a"]["version"], "0.5.3");
        assert_eq!(resp.body["slot_a"]["slot"], "a");
        assert_eq!(resp.body["slot_b"]["status"], "inactive");
        assert!(resp.body["slot_b"]["version"].is_null());
        // 内存态：state_path 为 null；last_check 初始 null
        assert!(resp.body["state_path"].is_null());
        assert!(resp.body["last_check"].is_null());
        assert!(resp.body["pending_updates"].is_array());
    }

    // ============ 10. semver 比较与 tag 解析纯函数 ============

    #[test]
    fn tag_parse_and_semver_compare() {
        // v 前缀剥离
        assert_eq!(
            parse_tag("v0.2.0").unwrap().as_string(),
            "0.2.0",
            "v 前缀应剥离"
        );
        assert!(parse_tag("0.2.0").is_some());
        assert!(parse_tag("windows-m1").is_none(), "非 semver tag 忽略");
        assert!(parse_tag("v1.2").is_none(), "两段版本非法");
        // semver：预发布 < 正式（os_update::version 语义直查）
        assert_eq!(
            os_update::version::compare_versions("0.3.0-beta", "0.3.0").unwrap(),
            std::cmp::Ordering::Less
        );
        assert_eq!(
            os_update::version::compare_versions("0.4.0-rc1", "0.3.0").unwrap(),
            std::cmp::Ordering::Greater
        );
        // 通道过滤纯函数
        let stable = Version::parse("0.2.0").unwrap();
        let beta = Version::parse("0.3.0-beta").unwrap();
        let rc = Version::parse("0.4.0-rc1").unwrap();
        assert!(channel_allows("stable", &stable));
        assert!(!channel_allows("stable", &beta));
        assert!(!channel_allows("stable", &rc));
        assert!(!channel_allows("beta", &stable));
        assert!(channel_allows("beta", &beta));
        assert!(channel_allows("nightly", &stable));
        assert!(channel_allows("nightly", &rc));
        assert!(channel_allows("manual", &beta));
        // tag 归属桶
        assert_eq!(tag_bucket(&stable), "stable");
        assert_eq!(tag_bucket(&beta), "beta");
        assert_eq!(tag_bucket(&rc), "prerelease");
        // filter_available：降序 + 过滤旧版/非 semver
        let tags = vec![
            ("v0.0.9".to_string(), None),
            ("v0.1.0".to_string(), None),
            (
                "v0.2.0".to_string(),
                Some("2026-08-01T00:00:00+08:00".into()),
            ),
            ("windows-m1".to_string(), None),
        ];
        let cur = Version::parse("0.1.0").unwrap();
        let out = filter_available(&tags, &cur, "nightly");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].version, "0.2.0");
        assert_eq!(
            out[0].created_at.as_deref(),
            Some("2026-08-01T00:00:00+08:00")
        );
    }

    // ============ 11. 路由声明与鉴权 ============

    #[tokio::test]
    async fn routes_declared_and_auth_conventions() {
        let h = UpdateRouteHandler::with_config(None, "/nonexistent/nexos.git", "0.1.0");
        let routes = h.routes().await;
        assert_eq!(routes.len(), 10, "应有 10 条路由: {routes:?}");
        assert!(
            routes.iter().all(|r| r.handler_component == "update"),
            "全部归属 update 组件"
        );
        for r in &routes {
            if r.method == HttpMethod::Post {
                assert!(r.requires_auth, "写操作需 auth: {r:?}");
                assert_eq!(r.required_roles, vec!["admin".to_string()]);
            } else {
                assert!(!r.requires_auth, "读端点公开: {r:?}");
            }
        }
    }

    // ============ 12. channels 目录 ============

    #[tokio::test]
    async fn channels_catalog_shape() {
        let h = UpdateRouteHandler::with_config(None, "/nonexistent/nexos.git", "0.1.0");
        let resp = h.handle(get_req("/api/v1/update/channels")).await.unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["current"], "stable");
        let list = resp.body["channels"].as_array().unwrap();
        assert_eq!(list.len(), 4, "四通道: {list:?}");
        let ids: Vec<&str> = list.iter().map(|c| c["id"].as_str().unwrap()).collect();
        assert_eq!(ids, vec!["stable", "beta", "nightly", "manual"]);
        for c in list {
            assert!(!c["name"].as_str().unwrap().is_empty());
            assert!(!c["description"].as_str().unwrap().is_empty());
        }
    }

    // ============ 13. 状态 JSON 原子写落盘格式 ============

    #[test]
    fn persist_state_atomic_write_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "nexos-update-persist-{}-{}",
            std::process::id(),
            FIXTURE_SEQ.fetch_add(1, Ordering::SeqCst)
        ));
        let _guard = TempDirGuard(dir.clone());
        // 目录不存在（含嵌套）→ 自动创建
        let path = dir.join("nested/os-data/update-state.json");
        let st = PersistState {
            channel: "nightly".into(),
            last_check: Some("2026-08-24T10:00:00+08:00".into()),
            available: vec![AvailableUpdate {
                tag: "v0.3.0-beta".into(),
                version: "0.3.0-beta".into(),
                channel: "beta".into(),
                created_at: None,
            }],
            artifacts: Vec::new(),
            tasks: Vec::new(),
        };
        persist_state_to(path.to_str().unwrap(), &st).expect("原子写应成功（目录自建）");
        assert!(path.exists(), "目标文件应存在");
        assert!(
            !std::path::Path::new(&format!("{}.tmp", path.display())).exists(),
            "临时文件应被 rename 走"
        );
        let back = load_state_from(path.to_str().unwrap());
        assert_eq!(back.channel, "nightly");
        assert_eq!(back.available.len(), 1);
        assert_eq!(back.available[0].tag, "v0.3.0-beta");
        // 缺失文件 → 缺省态
        let dflt = load_state_from("/nonexistent/update-state.json");
        assert_eq!(dflt.channel, "stable");
        assert!(dflt.tasks.is_empty());
    }

    // ============ 默认 trait ============

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<UpdateRouteHandler>();
    }
}
