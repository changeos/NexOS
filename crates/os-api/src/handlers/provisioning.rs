//! `ProvisioningRouteHandler` —— 系统自举（System Provisioning）桌面应用的
//! HTTP→内存态适配器，把原 PXE 启动服务重构为"系统自举"父应用，新增 ISO 生成
//! 与 SSH 远程部署两个子项。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/provisioning/*`）翻译为系统自举编排，
//! 返回 JSON。这是 OS"系统自举"桌面应用（PXE 网络启动 / ISO 镜像生成 /
//! SSH 远程部署）的后端 REST 入口。
//!
//! # 三个功能域
//!
//! - **PXE 启动（`pxe/*`）**：管理 PXE 配置 / 启动条目 / 服务运行态（搬自原
//!   `pxe.rs`，路由前缀改为 `/api/v1/provisioning/pxe/*`，逻辑完全一致）。
//! - **ISO 生成（`iso/*`）**：管理 ISO 构建任务并**真实驱动构建**——
//!   `POST /iso/tasks/:id/build` 经 os-iso 的 `XorrisoIsoBuilder` spawn
//!   mksquashfs / xorriso / sha256sum 子进程（`LoggingIsoRunner` 包装并记录
//!   每步命令与输出摘要），任务状态 pending→building→completed/failed
//!   轮询可查；工具链缺失（`IsoEnvironment::probe` 探测 xorriso/mksquashfs）
//!   时任务直接 failed 并附安装指引。
//! - **SSH 远程部署（`ssh/*`）**：管理 SSH 目标（密钥认证，无密码字段）+
//!   部署任务。`test` 与 `deploy` 均真实调系统 `ssh`/`scp` 子进程
//!   （BatchMode=yes 禁密码交互）：deploy 逐文件 scp（远端目录缺失先
//!   `ssh mkdir -p`），可选 run_cmd 经 `ssh ... sh -c '<cmd>'` 执行，
//!   状态机 pending→transferring→(running)→completed/failed 逐步落任务
//!   记录（文件级成败/耗时/输出），同目标同时只允许一个部署任务（409），
//!   单文件传输 300s / 远程命令 120s 超时（kill_on_drop 强杀）。
//! - **统计（`stats`）**：聚合三个域的关键计数。
//!
//! # 一键安装引导（`install.sh` / `prepare-distributable` / `dist/*`）
//!
//! 让一台 NAT 后的全新 Ubuntu 机器**一条 curl 命令**完成安装并自动加入集群
//! （docs/BOOTSTRAP_INSTALL.md）：
//!
//! ```ignore
//! sudo bash -c "$(curl -fsSL http://<任一公网入口>:8558/api/v1/provisioning/install.sh)"
//! ```
//!
//! 三端点协作：
//! - **`GET install.sh`（公开）**：按请求动态生成安装脚本——安装源 URL 由
//!   HTTP Host 头推导（任一节点都能当安装源）、P2P bootstrap 列表由源节点
//!   通告地址（env `NEXOS_GIT_ADVERTISE_HOST` / `NEXOS_P2P_ADVERTISE`）+
//!   固定公网入口拼出，新节点装好后第一交互对象即该公网入口；响应经网关
//!   直传通道（text/*，见 `crate::http::direct_passthrough_bytes`）返回
//!   未加 JSON 引号的脚本文本，`curl | bash` 即可用。
//! - **`POST prepare-distributable`（admin）**：把当前 os-api 可执行文件
//!   复制到约定分发路径 `/tank/os-data/latest-os-api.bin`（tmp+rename 原子
//!   替换）并流式计算 sha256——源节点侧发布新版本只需重跑一次（幂等）。
//!   Web 前端无需单独分发：rust-embed 已把 Vue3 产物内嵌进二进制。
//!   成功后**自动登记同版本更新工件**（2026-09-03：经装配层注入的共享
//!   update 实例，version=运行二进制 `CARGO_PKG_VERSION`、path=分发产物，
//!   复用 `POST /update/artifact` 全套校验 + sha256，重复 version 覆盖）
//!   ——发版流程"三节点各跑一次 prepare"即同时喂饱 dist 下载与页内
//!   apply 两条更新通道，登记结果随响应回传（`update_artifact` 字段）。
//! - **`GET dist/:artifact`（公开）**：分发下载通道——body 为标准 base64、
//!   `content-type: application/octet-stream`，经网关直传解码后客户端拿到
//!   原始字节（`curl -o` 即存盘）；artifact 走精确白名单（防穿越），并带
//!   `x-nexos-sha256` 响应头供安装脚本完整性对拍。白名单双架构：`os-api`
//!   （x86_64，prepare-distributable 暂存件）与 `os-api-aarch64`（ARM，
//!   `scripts/release.sh` 刷新到 `/tank/os-data/dist/os-api-aarch64-latest`）。
//!
//! # 实现策略：内存态 + 后台子进程
//!
//! 持有内存态 Mutex（`pxe_config` / `boot_entries` / `pxe_running` / `iso_tasks`
//! / `ssh_targets` / `deploy_tasks`），构造时预置示例数据，使各 `GET` 首次即
//! 返回非空 JSON。部署与 ISO 构建是长操作：POST 立即返回任务，`tokio::spawn`
//! 后台执行，前端轮询 `GET .../deploy/:id` / `GET .../iso/tasks/:id` 取真实
//! 进度（文件级结果 / 构建日志 / 产物路径）。
//!
//! # 安全
//!
//! - SSH 目标无 password 字段（红线），密钥认证 + BatchMode。
//! - 部署任务详情含远程路径与命令输出，读端点（`GET /ssh/deploys*`）
//!   要求 admin（其余 GET 维持公开）。
//! - 子进程输出各截 8KB（`DEPLOY_OUTPUT_CAP`），防内存放大。
//!
//! # 网络装机 P0（`bootstrap.ipxe` / `ipxe/menu` / `seed` / `isos` / 流式直传）
//!
//! U 盘 iPXE → 输 NexOS IP → 服务端动态菜单（按 ISO 仓实时生成）→ HTTP 拉
//! vmlinuz/initrd/完整 ISO → autoinstall 无人值守 → `late-commands` 跑
//! install.sh 自动入集群（docs/NET_BOOT.md；纯逻辑契约在 os-provision
//! `netboot`/`iso9660`，本节为 HTTP 侧自包含实现——PXE 域"搬自 pxe.rs"同款双轨）。
//!
//! - `GET bootstrap.ipxe`：dhcp → prompt 输服务器 IP（回车=源 IP）→ chain 菜单；
//! - `GET ipxe/menu?arch=&platform=`：按仓内**最新稳定版**渲染（滚动版本策略：
//!   每架构只列 latest；老版本仓管理手动装机）+ Clonezilla 占位灰显；
//! - `GET seed/:id/{meta-data,user-data}`：nocloud 种子（24.04/26.04 版本差异
//!   参数化；late-commands 闭环 install.sh）；
//! - `GET /isos`（清单：版本/架构/size/sha256/提取状态）+ `POST /isos/scan`
//!   （登记 + 后台 sha256 对账/写 sidecar + casper 提取）+ `DELETE /isos/:file`
//!   （滚动策略删旧版）；
//! - `GET /isos/:file` 与 `GET /boot/:id/:file`：**tokio::fs 流式直传特挂路由**
//!   （http.rs build_router，Range 206/416 + 白名单防穿越；3GB ISO 不进 base64
//!   网关 body——`direct_passthrough_bytes` 的流式扩展）。
//!
//! # 路由表（31 条）
//!
//! | method | path                                            | 动作 |
//! |--------|-------------------------------------------------|------|
//! | GET    | `/api/v1/provisioning/install.sh`               | 一键安装脚本（公开，动态生成，原文直传）|
//! | POST   | `/api/v1/provisioning/prepare-distributable`    | 发布可分发二进制（admin）|
//! | GET    | `/api/v1/provisioning/dist/:artifact`           | 分发下载（公开，base64→原始字节直传）|
//! | GET    | `/api/v1/provisioning/bootstrap.ipxe`           | 网络装机引导脚本（公开，text/plain）|
//! | GET    | `/api/v1/provisioning/ipxe/menu`                | 动态装机菜单（公开，按仓实时渲染）|
//! | GET    | `/api/v1/provisioning/seed/:id/meta-data`       | nocloud 空 meta（公开）|
//! | GET    | `/api/v1/provisioning/seed/:id/user-data`       | autoinstall 种子（公开）|
//! | GET    | `/api/v1/provisioning/isos`                     | ISO 仓清单（公开）|
//! | POST   | `/api/v1/provisioning/isos/scan`                | 仓登记+校验+提取触发（admin）|
//! | DELETE | `/api/v1/provisioning/isos/:file`               | 删旧版（admin，滚动策略）|
//! | GET    | `/api/v1/provisioning/isos/:file`               | ISO 流式直传（公开，axum 特挂，Range）|
//! | GET    | `/api/v1/provisioning/boot/:id/:file`           | casper 引导件流式直传（公开，axum 特挂）|
//! | GET    | `/api/v1/provisioning/pxe/config`               | PXE 配置 |
//! | POST   | `/api/v1/provisioning/pxe/config`               | 更新配置（admin）|
//! | GET    | `/api/v1/provisioning/pxe/boot-entries`         | 启动条目列表 |
//! | POST   | `/api/v1/provisioning/pxe/boot-entries`         | 添加条目（admin）|
//! | DELETE | `/api/v1/provisioning/pxe/boot-entries/:id`     | 删条目（admin）|
//! | GET    | `/api/v1/provisioning/pxe/status`               | PXE 服务状态 |
//! | POST   | `/api/v1/provisioning/pxe/start`                | 启动 PXE（admin）|
//! | POST   | `/api/v1/provisioning/pxe/stop`                 | 停止 PXE（admin）|
//! | GET    | `/api/v1/provisioning/iso/tasks`                | ISO 任务列表 |
//! | POST   | `/api/v1/provisioning/iso/tasks`                | 建 ISO 任务（admin）|
//! | DELETE | `/api/v1/provisioning/iso/tasks/:id`            | 删 ISO 任务（admin）|
//! | GET    | `/api/v1/provisioning/iso/tasks/:id`            | ISO 任务详情 |
//! | POST   | `/api/v1/provisioning/iso/tasks/:id/build`      | 启动真实构建（admin）|
//! | GET    | `/api/v1/provisioning/ssh/targets`              | SSH 目标列表 |
//! | POST   | `/api/v1/provisioning/ssh/targets`              | 添加 SSH 目标（admin）|
//! | DELETE | `/api/v1/provisioning/ssh/targets/:id`          | 删 SSH 目标（admin）|
//! | POST   | `/api/v1/provisioning/ssh/targets/:id/test`     | 测试 SSH 连接（admin）|
//! | POST   | `/api/v1/provisioning/ssh/deploy`               | 发起部署（admin，真实执行）|
//! | GET    | `/api/v1/provisioning/ssh/deploys`              | 部署任务列表（admin）|
//! | GET    | `/api/v1/provisioning/ssh/deploy/:id`           | 部署任务状态（admin）|
//! | GET    | `/api/v1/provisioning/stats`                    | 聚合统计 |

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::update::UpdateRouteHandler;
use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};
use crate::handlers::tasks_common::{UnifiedStatus, UnifiedTask, UnifiedTaskStore};
use os_iso::runner::{IsoBuildRunner, ProcessOutput};
use os_iso::{IsoBuildStatus, IsoBuilder, IsoError, IsoSpec, IsoVariant, XorrisoIsoBuilder};

// ----------------------------------------------------------------------------
// 常量
// ----------------------------------------------------------------------------

/// SSH/SCP 公共参数：BatchMode 禁密码交互 + 连接超时 + 首连自动接受新主机密钥。
const SSH_BASE_OPTS: [&str; 6] = [
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "StrictHostKeyChecking=accept-new",
];

/// SSH 私钥缺省路径（与目标未填 private_key_path 时一致）。
const DEFAULT_SSH_KEY: &str = "~/.ssh/id_ed25519";

/// 部署子进程输出捕获上限（stdout/stderr 各 8KB，防内存放大）。
const DEPLOY_OUTPUT_CAP: usize = 8 * 1024;

/// 单文件传输超时（scp）。
const DEPLOY_FILE_TIMEOUT: Duration = Duration::from_secs(300);

/// 远程命令 / mkdir 超时（ssh）。
const DEPLOY_CMD_TIMEOUT: Duration = Duration::from_secs(120);

/// SSH test 连接整体超时（ConnectTimeout 只管 TCP，这里兜住整条命令）。
const SSH_TEST_TIMEOUT: Duration = Duration::from_secs(15);

/// 部署任务记录上限（内存态防膨胀，超出丢弃最旧）。
const DEPLOY_TASKS_MAX: usize = 100;

/// ISO 构建日志行数上限（超出丢最旧）。
const ISO_LOG_MAX_LINES: usize = 500;

// ----------------------------------------------------------------------------
// 常量 —— 一键安装引导（install.sh / prepare-distributable / dist）
// ----------------------------------------------------------------------------

/// 可分发工件的约定目录（env `NEXOS_DISTRIBUTABLE_DIR` 覆盖；缺省
/// `/tank/os-data`——identity-ledger / update-state 等运行期数据的既有根）。
const DEFAULT_DISTRIBUTABLE_DIR: &str = "/tank/os-data";

/// 可分发 os-api 二进制在分发目录内的固定文件名（x86_64 主件）。
const DISTRIBUTABLE_BIN_NAME: &str = "latest-os-api.bin";

/// aarch64 工件在分发目录内的固定相对路径（`scripts/release.sh` 双架构
/// 构建后刷新到此；DGX Spark 等 ARM 机器经 `dist/os-api-aarch64` 下载）。
const DISTRIBUTABLE_AARCH64_REL_PATH: &str = "dist/os-api-aarch64-latest";

/// aarch64 工件的下载名（白名单项；与 [`artifact_fs_path`] 映射共用常量）。
pub const DISTRIBUTABLE_AARCH64_ARTIFACT: &str = "os-api-aarch64";

/// `GET dist/:artifact` 工件白名单（**防穿越核心**：只认精确名，
/// 含 `/`、`\`、`..`、百分号编码等任何形态的路径注入都无法命中）。
/// `os-api` = x86_64 主件（prepare-distributable 暂存）；
/// `os-api-aarch64` = ARM 件（release.sh 刷新的分发目录产物）。
pub const DISTRIBUTABLE_ARTIFACTS: [&str; 2] = ["os-api", DISTRIBUTABLE_AARCH64_ARTIFACT];

/// 公网 P2P 入口（bootstrap 缺省列表的固定补充项；新节点入网的第一交互对象）。
pub const PUBLIC_ENTRY_ALIYUN: &str = "203.0.113.2:7070";

/// 公网 P2P 入口——云锚点（同上，第二引导位）。
pub const PUBLIC_ENTRY_ANCHOR: &str = "198.51.100.114:7070";

/// os-api HTTP 端口缺省约定（生产 unit `--addr 0.0.0.0:8558`）。
const DEFAULT_API_PORT: u16 = 8558;

/// P2P 监听端口缺省约定（`NEXOS_P2P_LISTEN=:7070`）。
const DEFAULT_P2P_PORT: u16 = 7070;

// ----------------------------------------------------------------------------
// 常量 —— 网络装机 P0（ISO 仓 / iPXE 引导链 / autoinstall 种子 / 流式直传）
// ----------------------------------------------------------------------------

/// 网络装机 ISO 仓根目录（env `NEXOS_PROVISION_REPO` 覆盖；缺省
/// `/tank/os-data/provision`——与 106 tank 数据根同源）。布局约定：
/// `<root>/isos/ubuntu-<ver>-live-server-<arch>.iso`（ISO 仓件 +
/// `<file>.sha256` 对账 sidecar）+ `<root>/boot/<id>/{vmlinuz,initrd}`
/// （ISO 内 casper 提取件）+ `<root>/isos/ipxe.iso`（官方引导介质）。
pub const DEFAULT_NETBOOT_REPO: &str = "/tank/os-data/provision";

/// 仓内每件 ISO 的 sha256 sidecar 后缀（`<iso>.sha256`，内容
/// `<hex>  <file>`——与 sha256sum 输出同形）。
const ISO_SHA256_SIDECAR_SUFFIX: &str = ".sha256";

/// 流式直传分块大小（256KiB——ISO 直传与 casper Range 断点拉取的吞吐/内存折中）。
pub const NETBOOT_STREAM_CHUNK: usize = 256 * 1024;

/// 引导介质（U 盘件）白名单：仓 isos/ 内除 OS ISO 外允许直传的精确文件名
/// （官方 ipxe.iso，供自举页下载制作 U 盘）。
pub const NETBOOT_BOOT_MEDIA_FILES: [&str; 1] = ["ipxe.iso"];

// ----------------------------------------------------------------------------
// DTO —— PXE（搬自 pxe.rs，字段改 pub）
// ----------------------------------------------------------------------------

/// PXE 配置状态（内存态）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PxeConfig {
    /// 是否启用 PXE
    pub enabled: bool,
    /// TFTP 服务器 IP（DHCP next-server）
    pub tftp_server: String,
    /// 引导模式：`"bios"` / `"uefi"` / `"uefi_arm64"`
    pub boot_mode: String,
    /// HTTP 镜像仓库地址
    pub http_repo: String,
    /// 默认 bootfile（如 `"pxelinux.0"` / `"ipxe.efi"`）
    pub default_bootfile: String,
}

/// 一条 PXE 启动条目（内核 + initramfs + cmdline）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BootEntry {
    /// 条目 ID（唯一，用于 DELETE 定位）
    pub id: String,
    /// 展示名
    pub name: String,
    /// 内核路径
    pub kernel: String,
    /// initramfs 路径
    pub initrd: String,
    /// 内核命令行参数
    pub cmdline: String,
    /// 是否为默认启动项
    #[serde(rename = "default")]
    pub default_entry: bool,
}

/// PXE 服务状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PxeStatus {
    pub running: bool,
    pub state: String,
}

// ----------------------------------------------------------------------------
// DTO —— ISO 生成
// ----------------------------------------------------------------------------

/// ISO 构建任务（`POST /iso/tasks/:id/build` 后由真实子进程驱动状态机）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IsoTask {
    pub id: String,
    pub name: String,
    /// 版本号，如 "1.0.0"
    pub version: String,
    /// 变体：std / clone
    pub variant: String,
    /// 架构：x86_64 / aarch64
    pub arch: String,
    /// Ubuntu 基础版本，如 "26.04"
    pub ubuntu_version: String,
    /// 状态：pending / building / completed / failed
    pub status: String,
    /// 产物路径（completed 时）
    pub iso_path: Option<String>,
    pub sha256: Option<String>,
    pub size_bytes: Option<u64>,
    pub created_at: String,
    pub error: Option<String>,
    /// 构建当前步骤（building 时，如 "mksquashfs" / "xorriso"）
    #[serde(default)]
    pub step: Option<String>,
    /// 构建进度 0.0~1.0（building 时）
    #[serde(default)]
    pub progress: Option<f32>,
    /// 构建日志（每步命令 + 退出码 + 输出摘要；building 时为实时，终态后为快照）
    #[serde(default)]
    pub build_log: Vec<String>,
}

/// `GET /api/v1/provisioning/stats` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvisioningStats {
    pub pxe_running: bool,
    pub iso_tasks_total: usize,
    pub iso_completed: usize,
    pub iso_failed: usize,
    pub ssh_targets_total: usize,
    pub ssh_reachable: usize,
    /// 部署任务总数（内存态保留的最近记录）
    pub deploys_total: usize,
}

// ----------------------------------------------------------------------------
// DTO —— SSH 远程部署（红线：无 password 字段，纯密钥认证）
// ----------------------------------------------------------------------------

/// SSH 目标主机（密钥认证，无密码）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshTarget {
    pub id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub user: String,
    /// 私钥路径；None 时后端用默认 `~/.ssh/id_ed25519`
    pub private_key_path: Option<String>,
    /// 状态：unknown / reachable / unreachable
    pub status: String,
    pub last_checked: Option<String>,
    pub created_at: String,
}

/// 一份文件传输（local → remote）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileTransfer {
    pub local_path: String,
    pub remote_path: String,
}

/// 单文件传输结果（文件级 ✓/✗，逐步落任务记录）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileTransferResult {
    pub local_path: String,
    pub remote_path: String,
    /// pending / success / failed / skipped
    pub status: String,
    /// scp 退出码（超时被杀为 -1）
    pub exit_code: Option<i32>,
    /// 传输耗时（毫秒）
    pub duration_ms: Option<u64>,
    /// 失败原因（stderr 摘要 / 超时说明）
    pub error: Option<String>,
}

/// 远程命令执行结果（stdout/stderr 各截 8KB）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CmdOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
}

/// SSH 部署任务（真实执行：scp 逐文件 + 可选 ssh 远程命令）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployTask {
    pub id: String,
    pub target_id: String,
    pub files: Vec<FileTransfer>,
    pub run_cmd: Option<String>,
    /// 状态：pending / transferring / running / completed / failed
    pub status: String,
    pub created_at: String,
    pub error: Option<String>,
    /// 文件级结果（与 files 对齐；逐步更新）
    #[serde(default)]
    pub results: Vec<FileTransferResult>,
    /// run_cmd 执行结果（有命令且执行过时）
    #[serde(default)]
    pub cmd_output: Option<CmdOutput>,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub finished_at: Option<String>,
}

// ----------------------------------------------------------------------------
// 请求体（POST）
// ----------------------------------------------------------------------------

/// 创建 ISO 任务请求体。
#[derive(Debug, Deserialize)]
struct CreateIsoTaskBody {
    name: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    variant: Option<String>,
    #[serde(default)]
    arch: Option<String>,
    #[serde(default)]
    ubuntu_version: Option<String>,
}

/// 添加 SSH 目标请求体。
#[derive(Debug, Deserialize)]
struct CreateSshTargetBody {
    name: String,
    host: String,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    private_key_path: Option<String>,
}

/// 发起部署请求体。
#[derive(Debug, Deserialize)]
struct CreateDeployBody {
    target_id: String,
    #[serde(default)]
    files: Vec<FileTransfer>,
    #[serde(default)]
    run_cmd: Option<String>,
}

/// 一份就绪的可分发工件（`POST prepare-distributable` 响应 / 安装源元数据）。
#[derive(Debug, Clone, Serialize)]
pub struct PreparedArtifact {
    /// 分发路径（绝对路径，供 Files API 兜底通道使用）
    pub path: String,
    /// 字节数
    pub size_bytes: u64,
    /// 内容 sha256（hex；脚本生成时烘焙进 install.sh 供下载端完整性对拍）
    pub sha256: String,
    /// 客户端取用路径（相对安装源的 URL path）
    pub download_path: String,
    /// prepare 顺手登记的同版本更新工件（2026-09-03：version=运行二进制
    /// `CARGO_PKG_VERSION`、path=本 path——复用 POST /update/artifact 全套
    /// 校验+sha256；页内「应用更新」自此 prepare 后即可用）。None/缺省 =
    /// 未注入 update 登记通道（单测构造）或登记失败（分发主通道不受影响，
    /// 失败原因见 `update_artifact_error`）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_artifact: Option<crate::handlers::update::UpdateArtifact>,
    /// 自动登记失败原因（`update_artifact` 缺席时有值；prepare 本身仍成功）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_artifact_error: Option<String>,
}

// ----------------------------------------------------------------------------
// 纯函数：ISO 产物命名规则
// ----------------------------------------------------------------------------

/// 构造 ISO 产物文件名：`os-{variant}-{ubuntu_version}-{version}.iso`。
///
/// 例：variant=std, ubuntu_version=26.04, version=1.0.0 → `os-std-26.04-1.0.0.iso`。
#[must_use]
pub fn iso_filename(task: &IsoTask) -> String {
    format!(
        "os-{}-{}-{}.iso",
        task.variant, task.ubuntu_version, task.version
    )
}

// ----------------------------------------------------------------------------
// 一键安装引导：纯函数辅助（路径 / bootstrap 列表 / 安装源 URL / 工件暂存）
// ----------------------------------------------------------------------------

/// 可分发工件目录（env `NEXOS_DISTRIBUTABLE_DIR` 覆盖，测试注入用）。
#[must_use]
pub fn distributable_dir() -> PathBuf {
    std::env::var_os("NEXOS_DISTRIBUTABLE_DIR")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_DISTRIBUTABLE_DIR))
}

/// 可分发 os-api 二进制的固定路径。
#[must_use]
pub fn distributable_bin_path() -> PathBuf {
    distributable_dir().join(DISTRIBUTABLE_BIN_NAME)
}

/// 白名单工件名 → 分发目录内固定文件路径（**防穿越常量表**：调用方必须
/// 先过 [`DISTRIBUTABLE_ARTIFACTS`] 精确白名单；映射本身只认常量名、
/// 不做任何用户输入拼接，未知名兜底回落 x86_64 主件路径）。
#[must_use]
pub fn artifact_fs_path(artifact: &str) -> PathBuf {
    match artifact {
        DISTRIBUTABLE_AARCH64_ARTIFACT => distributable_dir().join(DISTRIBUTABLE_AARCH64_REL_PATH),
        _ => distributable_bin_path(),
    }
}

/// host 规格化为 P2P `host:7070` 端点（已带端口原样返回）。
fn normalized_p2p_endpoint(host: &str) -> String {
    let host = host.trim();
    if host.is_empty() {
        return String::new();
    }
    if host.rfind(':').is_some() {
        // 已带端口（IPv4/IPv6:[port]/域名:port 形态）
        return host.to_string();
    }
    format!("{host}:{DEFAULT_P2P_PORT}")
}

/// 缺省 P2P bootstrap 列表：源节点通告地址优先，其后为固定公网入口。
///
/// 通告地址取 env `NEXOS_GIT_ADVERTISE_HOST` / `NEXOS_P2P_ADVERTISE`
/// （跳过回环/未指定地址——与 os-p2p 的 unspecified 守卫同一惯例），保证
/// **新节点装好后的第一交互对象是发起安装的公网入口**。去重后逗号连接。
#[must_use]
pub fn default_bootstrap_list() -> String {
    let advertise = std::env::var("NEXOS_GIT_ADVERTISE_HOST")
        .ok()
        .or_else(|| std::env::var("NEXOS_P2P_ADVERTISE").ok())
        .unwrap_or_default();
    let mut list = Vec::new();
    let first = normalized_p2p_endpoint(&advertise);
    if !first.is_empty() && !first.contains("0.0.0.0") && !first.contains("127.") {
        list.push(first);
    }
    for entry in [PUBLIC_ENTRY_ALIYUN, PUBLIC_ENTRY_ANCHOR] {
        if !list.iter().any(|e| e == entry) {
            list.push(entry.to_string());
        }
    }
    list.join(",")
}

/// 从请求推导本节点的安装源 base URL（供脚本下载二进制/自引用）：
/// HTTP Host 头最准（任一节点都能当安装源）；缺失时回退通告 host + 缺省端口，
/// 再回退本地回环（脚本侧会因下载失败给出 prepare 指引，不静默错装）。
fn source_base_url(req: &ApiRequest) -> String {
    if let serde_json::Value::Object(map) = &req.headers {
        if let Some((_, v)) = map.iter().find(|(k, _)| k.eq_ignore_ascii_case("host")) {
            if let Some(h) = v.as_str() {
                let h = h.trim();
                if !h.is_empty() {
                    return format!("http://{h}");
                }
            }
        }
    }
    let advertise = std::env::var("NEXOS_GIT_ADVERTISE_HOST")
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if advertise.is_empty() {
        format!("http://127.0.0.1:{DEFAULT_API_PORT}")
    } else {
        format!("http://{advertise}:{DEFAULT_API_PORT}")
    }
}

/// 把当前可执行文件暂存到分发路径：单遍流式复制 + sha256（64KiB 分块读），
/// 先写同目录临时文件再 rename 原子替换（半程失败不留残缺分发件）。
///
/// 返回 [`PreparedArtifact`]；exe 不可读 / 目标目录不可写 → Err 说明串。
pub fn stage_artifact(
    exe: &std::path::Path,
    out: &std::path::Path,
) -> Result<PreparedArtifact, String> {
    use std::io::{Read as _, Write as _};
    let mut src = std::fs::File::open(exe).map_err(|e| format!("读取当前可执行文件失败: {e}"))?;
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建分发目录失败: {e}"))?;
    }
    let tmp_path = out.with_extension("tmp.part");
    let mut tmp =
        std::fs::File::create(&tmp_path).map_err(|e| format!("创建临时分发文件失败: {e}"))?;
    let mut hasher = Sha256::new();
    let mut size: u64 = 0;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = src
            .read(&mut buf)
            .map_err(|e| format!("读取可执行文件失败: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
        tmp.write_all(&buf[..n])
            .map_err(|e| format!("写临时分发文件失败: {e}"))?;
    }
    drop(tmp);
    std::fs::rename(&tmp_path, out).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        format!("原子替换分发文件失败: {e}")
    })?;
    Ok(PreparedArtifact {
        path: out.display().to_string(),
        size_bytes: size,
        sha256: format!("{:x}", hasher.finalize()),
        download_path: "/api/v1/provisioning/dist/os-api".to_string(),
        // 登记由调用方（HTTP handler）在 stage 成功后补写——纯函数层只管暂存。
        update_artifact: None,
        update_artifact_error: None,
    })
}

/// 读取一份待分发的工件：整读入内存并计算 sha256（Files API 的 read_download
/// 同款整读先例）。不可读（不存在等）→ None，由调用方给"先 prepare"指引。
fn load_artifact(path: &std::path::Path) -> Option<(Vec<u8>, String)> {
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Some((bytes, format!("{:x}", hasher.finalize())))
}

/// 把单引号从嵌入值中剥除（值只进 bash 单引号字面量，防注入的最小处理；
/// URL/host/bootstrap 都是机器形态，正常输入不含 `'`）。
fn strip_single_quotes(s: &str) -> String {
    s.replace('\'', "")
}

// ----------------------------------------------------------------------------
// 网络装机 P0：ISO 仓 / 引导链 / 种子 / 流式直传（docs/NET_BOOT.md）
//
// 纯逻辑契约与先导实现在 `crates/os-provision/src/{netboot,iso9660}.rs`（含真实
// ISO 提取校验测）；os-api 与 os-provision 间无依赖边（Cargo.toml 不动），本节
// 为 HTTP 侧自包含实现——与既有先例一致（PXE 配置/启动条目即"搬自 pxe.rs、
// 逻辑完全一致"的双轨）。白名单/YAML/iPXE 形态两侧对齐，os-api 侧另有独立测试。
// ----------------------------------------------------------------------------

/// 支持的仓架构（与 Ubuntu live-server 文件名一致）。
pub const NETBOOT_ARCHES: [&str; 2] = ["amd64", "arm64"];

/// ISO 仓文件名白名单解析：只认 `ubuntu-<点分数字>-live-server-<amd64|arm64>.iso`
/// 精确形态（**防穿越核心**：任何 `/`、`\`、`..`、大写、异架构形态都 None）。
/// 返回 (version, arch, id)。
#[must_use]
pub fn parse_repo_iso_filename(file: &str) -> Option<(String, String, String)> {
    let stem = file.strip_suffix(".iso")?;
    let rest = stem.strip_prefix("ubuntu-")?;
    let (version, arch) = rest.rsplit_once("-live-server-")?;
    let parts: Vec<&str> = version.split('.').collect();
    let version_ok = parts.len() >= 2
        && version.len() <= 16
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    if !version_ok || !NETBOOT_ARCHES.contains(&arch) {
        return None;
    }
    if file.contains(['/', '\\', '\0']) || file.chars().any(|c| c.is_ascii_uppercase()) {
        return None;
    }
    Some((
        version.to_string(),
        arch.to_string(),
        format!("{version}-{arch}"),
    ))
}

/// Ubuntu 版本数值比较（点分段逐段比，缺段当 0）。
fn netboot_version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let pa: Vec<u64> = a.split('.').map(|s| s.parse().unwrap_or(0)).collect();
    let pb: Vec<u64> = b.split('.').map(|s| s.parse().unwrap_or(0)).collect();
    for i in 0..pa.len().max(pb.len()) {
        match pa
            .get(i)
            .copied()
            .unwrap_or(0)
            .cmp(&pb.get(i).copied().unwrap_or(0))
        {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}

/// ISO 仓根目录（env `NEXOS_PROVISION_REPO` 覆盖，测试注入用）。
#[must_use]
pub fn netboot_repo_root() -> PathBuf {
    std::env::var_os("NEXOS_PROVISION_REPO")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_NETBOOT_REPO))
}

/// `NEXOS_PROVISION_REPO` env 注入测试的全局串行锁（provisioning 与 http 两处
/// 测试模块共用——env 是进程级全局，改写窗口必须互斥）。
#[cfg(test)]
pub(crate) static NETBOOT_ENV_LOCK: Mutex<()> = Mutex::new(());

/// 仓 isos 目录。
#[must_use]
pub fn netboot_isos_dir() -> PathBuf {
    netboot_repo_root().join("isos")
}

/// 仓 boot 提取件目录（`boot/<id>/`——id 形如 `26.04.1-amd64`）。
#[must_use]
pub fn netboot_boot_dir(id: &str) -> PathBuf {
    netboot_repo_root().join("boot").join(id)
}

/// 引导介质（ipxe.iso）在仓内的白名单名 → 绝对路径。
#[must_use]
pub fn netboot_boot_media_path(name: &str) -> Option<PathBuf> {
    NETBOOT_BOOT_MEDIA_FILES
        .contains(&name)
        .then(|| netboot_isos_dir().join(name))
}

/// ISO 仓文件 → 绝对路径（**先过** [`parse_repo_iso_filename`]——本函数不做
/// 校验，调用方必须已排除路径注入形态）。
#[must_use]
pub fn netboot_iso_path(file: &str) -> PathBuf {
    netboot_isos_dir().join(file)
}

/// 网络装机来源 base（`http://<Host 头>`）——bootstrap.ipxe/菜单/boot 条目/种子
/// 里的 URL 全部由它渲染（目标机输入的 NexOS IP 即此来源，菜单链路同源自洽）。
/// Host 缺失回退通告地址/回环（与 `source_base_url` 同策略）。
fn netboot_base_url(req: &ApiRequest) -> String {
    source_base_url(req)
}

/// iPXE `buildarch` → 仓架构（`x86_64`/`i386` → amd64，`arm64`/`aarch64` → arm64）。
#[must_use]
pub fn map_ipxe_buildarch(buildarch: &str) -> &'static str {
    match buildarch.trim().to_ascii_lowercase().as_str() {
        "arm64" | "aarch64" => "arm64",
        _ => "amd64",
    }
}

// ----------------------------------------------------------------------------
// 网络装机：iPXE 引导链脚本渲染（bootstrap / 动态菜单）
// ----------------------------------------------------------------------------

/// 生成 `bootstrap.ipxe`：U 盘 iPXE 引导后的第一跳——dhcp → console → prompt
/// 输 NexOS 服务器 IP（直接回车 = 缺省 `${next-server}`，即源 IP）→ `chain`
/// 服务端动态菜单（菜单/版本/种子永远最新，U 盘只烧一次）。
#[must_use]
pub fn render_bootstrap_ipxe() -> String {
    let mut s = String::new();
    s.push_str("#!ipxe\n");
    s.push_str("# NexOS 网络装机引导（bootstrap.ipxe，服务端动态生成——勿手改）\n");
    s.push_str("# U 盘只需烧一次：菜单/版本/种子永远来自服务端（chain 下一跳）\n");
    s.push_str("console\n");
    s.push_str("dhcp net0 || true\n");
    s.push_str("# 缺省回车 = 源 IP（DHCP next-server / DHCP 服务器），通常即局域网 NexOS 节点\n");
    s.push_str("set nexos_srv ${next-server}\n");
    s.push_str("isset ${nexos_srv} || set nexos_srv ${net0/dhcp/server}\n");
    s.push_str(":ask\n");
    s.push_str("clear screen\n");
    s.push_str("echo === NexOS 网络装机 ===\n");
    s.push_str("echo 本机 ${platform}/${buildarch}  网卡 ${net0/mac}\n");
    s.push_str("echo 请输入局域网 NexOS 服务器 IP（直接回车 = ${nexos_srv}）\n");
    s.push_str("prompt --key 0x0d 'NexOS IP: ' && read nexos_srv || goto ask\n");
    s.push_str("isset ${nexos_srv} || goto ask\n");
    s.push_str(&format!(
        "chain --replace http://${{nexos_srv}}:{DEFAULT_API_PORT}/api/v1/provisioning/ipxe/menu?arch=${{buildarch}}&platform=${{platform}} || goto ask\n"
    ));
    s
}

/// 生成动态菜单脚本（按仓内**最新稳定版**实时渲染——滚动版本策略：自动装机只
/// 列每架构 latest；老版本/其他架构走仓管理手动装机）。Clonezilla 项 P0 占位
/// 灰显（`item --disabled`，P1 接通）。
fn render_ipxe_menu(
    items: &[(String, String, String)],
    arch: &str,
    platform: &str,
    base: &str,
) -> String {
    // items: (version, arch, id)——调用方已过滤 latest + arch 匹配
    let mut s = String::new();
    s.push_str("#!ipxe\n");
    s.push_str("# NexOS 动态装机菜单（服务端按 ISO 仓实时生成）\n");
    s.push_str(&format!(
        "# 本机 {platform}/{arch}（仓内最新稳定版才进自动装机——滚动版本策略）\n"
    ));
    s.push_str("console\n");
    s.push_str(&format!("menu NexOS 网络装机（{platform}/{arch}）\n"));
    if items.is_empty() {
        s.push_str("item --gap 仓内没有该架构的可装机 ISO\n");
        s.push_str("item --gap 请在 NexOS「系统自举→网络装机」登记 ISO 后重试\n");
    } else {
        for (version, _, _) in items {
            s.push_str(&format!(
                "item --default os{key} Ubuntu Server {version}（{arch}）\n",
                key = version.replace('.', "")
            ));
        }
    }
    s.push_str("item --gap\n");
    s.push_str("item --disabled clonezilla 克隆整机（Clonezilla，P1 提供——敬请期待）\n");
    s.push_str("item shell iPXE Shell（救援/诊断）\n");
    s.push_str("item reboot 重启计算机\n");
    s.push_str("choose --timeout 0 target && goto ${target}\n\n");
    for (version, _, id) in items {
        let key = version.replace('.', "");
        s.push_str(&format!(":os{key}\n"));
        s.push_str(&format!(
            "echo 即将网络安装 Ubuntu Server {version}（{arch}）：单盘将被全清，3 秒内可按 Ctrl+B 中止\n"
        ));
        s.push_str(&format!(
            "kernel {base}/api/v1/provisioning/boot/{id}/vmlinuz root=/dev/ram0 ramdisk_size=1500000 ip=dhcp url={base}/api/v1/provisioning/isos/ubuntu-{version}-live-server-{arch}.iso autoinstall ds=nocloud-net\\;s={base}/api/v1/provisioning/seed/{id}/\n"
        ));
        s.push_str(&format!(
            "initrd {base}/api/v1/provisioning/boot/{id}/initrd\n"
        ));
        s.push_str("boot\n\n");
    }
    s.push_str(":shell\n");
    s.push_str(
        "echo 输入 exit 返回菜单；也可手动 chain http://<ip>:8558/api/v1/provisioning/ipxe/menu\n",
    );
    s.push_str("shell ||\n");
    // shell 退出后重拉本菜单（base 已烘焙——菜单是 chain 来的，可无限自循环）
    s.push_str(&format!(
        "chain {base}/api/v1/provisioning/ipxe/menu?arch={arch}&platform={platform} ||\n"
    ));
    s.push_str(":reboot\n");
    s.push_str("reboot\n");
    s
}

// ----------------------------------------------------------------------------
// 网络装机：autoinstall nocloud 种子模板（版本/架构/来源参数化）
// ----------------------------------------------------------------------------

/// 缺省装机用户。
pub const DEFAULT_SEED_USERNAME: &str = "nexos";

/// 缺省密码 `nexos` 的 SHA-512 crypt（首启请改密；`openssl passwd -6` 生成）。
pub const DEFAULT_SEED_PASSWORD_CRYPT: &str =
    "$6$NexOSBoot$QRd9jbbAo8wEAwE3rAyHfSF9WMaSXhruQ7X.4l2ljkmuH.p7VEej6VKhSqjX96RNKO8AF8tP3ySdEHQQjvvYw1";

/// 缺省 apt 主镜像（国内）。
pub const DEFAULT_SEED_APT_MIRROR: &str = "http://mirrors.aliyun.com/ubuntu";

/// user-data 渲染参数（照 install.sh 模板的占位替换风格）。
#[derive(Debug, Clone)]
pub struct AutoinstallSeedParams {
    pub version: String,
    pub arch: String,
    /// 种子/install.sh 来源 base（`http://<NexOS IP>:8558`——Host 头推导）。
    pub server_base: String,
    pub hostname: String,
    pub username: String,
    pub password_crypt: String,
    pub apt_mirror: String,
}

impl Default for AutoinstallSeedParams {
    fn default() -> Self {
        Self {
            version: "26.04.1".to_string(),
            arch: "amd64".to_string(),
            server_base: format!("http://127.0.0.1:{DEFAULT_API_PORT}"),
            hostname: "nexos-node".to_string(),
            username: DEFAULT_SEED_USERNAME.to_string(),
            password_crypt: DEFAULT_SEED_PASSWORD_CRYPT.to_string(),
            apt_mirror: DEFAULT_SEED_APT_MIRROR.to_string(),
        }
    }
}

/// 是否 26.04+ 世代（apt `mirror-selection`/`geoip` 键；24.04 走旧 `primary`）。
fn seed_is_new_apt_schema(version: &str) -> bool {
    !matches!(
        netboot_version_cmp(version, "26.04"),
        std::cmp::Ordering::Less
    )
}

/// 渲染 nocloud `user-data`（`#cloud-config` autoinstall YAML）。
///
/// 要点：存储全自动单盘清除（direct + largest）；网络 dhcp；SSH 装服务端；
/// **版本差异**（26.04 显式 `geoip: false` + `mirror-selection`，24.04 旧
/// `primary`）；**闭环** late-commands `curtin in-target` 下载执行本 NexOS 的
/// install.sh（`--bootstrap` 指回发起装机节点 → 装完自动入集群）。
pub fn render_seed_user_data(p: &AutoinstallSeedParams) -> Result<String, String> {
    parse_repo_iso_filename(&format!("ubuntu-{}-live-server-{}.iso", p.version, p.arch))
        .ok_or_else(|| {
            format!(
                "autoinstall 参数非法: version={} arch={}",
                p.version, p.arch
            )
        })?;
    if !is_safe_seed_scalar(&p.hostname) || !is_safe_seed_scalar(&p.username) {
        return Err("hostname/username 含非法字符（仅字母数字连字符下划线点）".to_string());
    }
    let base = p.server_base.trim_end_matches('/');
    let bootstrap_host = p
        .server_base
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split(':')
        .next()
        .unwrap_or("");
    let mut s = String::new();
    s.push_str("#cloud-config\n");
    s.push_str("# NexOS 网络装机种子（nocloud user-data；服务端动态渲染）\n");
    s.push_str("autoinstall:\n");
    s.push_str("  version: 1\n");
    s.push_str("  locale: en_US.UTF-8\n");
    s.push_str("  keyboard: { layout: us }\n");
    s.push_str("  network:\n");
    s.push_str("    version: 2\n");
    s.push_str("    ethernets:\n");
    s.push_str("      any:\n");
    s.push_str("        match:\n");
    s.push_str("          name: en*\n");
    s.push_str("        dhcp4: true\n");
    s.push_str("  storage:\n");
    s.push_str("    layout:\n");
    s.push_str("      name: direct\n");
    s.push_str("      match:\n");
    s.push_str("        size: largest\n");
    s.push_str("  identity:\n");
    s.push_str(&format!("    hostname: {}\n", p.hostname));
    s.push_str(&format!("    username: {}\n", p.username));
    s.push_str(&format!("    password: '{}'\n", p.password_crypt));
    s.push_str("  ssh:\n");
    s.push_str("    install-server: true\n");
    s.push_str("    allow-pw: true\n");
    s.push_str("  apt:\n");
    if seed_is_new_apt_schema(&p.version) {
        s.push_str("    geoip: false\n");
        s.push_str("    mirror-selection:\n");
        s.push_str("      primary:\n");
        s.push_str(&format!("        - uri: {}\n", p.apt_mirror));
        s.push_str("          arches: [amd64, arm64]\n");
    } else {
        s.push_str("    primary:\n");
        s.push_str(&format!("      - uri: {}\n", p.apt_mirror));
        s.push_str("        arches: [amd64, arm64]\n");
    }
    s.push_str("    fallback: offline-install\n");
    s.push_str("  packages:\n");
    s.push_str("    - curl\n");
    s.push_str("    - openssh-server\n");
    s.push_str("  late-commands:\n");
    // 闭环：chroot 内下载 install.sh 落盘再执行（管道形式会把 bash 留在 live
    // 环境、写到错误的根）；--bootstrap 指回发起装机的 NexOS 节点
    s.push_str(&format!(
        "    - curtin in-target -- bash -c 'curl -fsSL {base}/api/v1/provisioning/install.sh -o /tmp/nexos-install.sh && bash /tmp/nexos-install.sh --source {base} --bootstrap {bootstrap_host}:{DEFAULT_P2P_PORT}'\n"
    ));
    s.push_str("  shutdown: reboot\n");
    Ok(s)
}

/// YAML 标量安全（字母数字 + `-_.`，≤64 字符；防引号/换行注入）。
fn is_safe_seed_scalar(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
}

// ----------------------------------------------------------------------------
// 网络装机：ISO 仓扫描 / sha256 对账 / casper 提取（自包含 ISO9660 读）
// ----------------------------------------------------------------------------

/// 一件仓内 ISO 的就绪状态（`GET /isos` 列表项 / scan 登记）。
#[derive(Debug, Clone, Serialize)]
pub struct IsoRepoFile {
    /// 仓内文件名（=下载名）。
    pub file: String,
    /// 版本（`26.04.1`）。
    pub version: String,
    /// 架构（`amd64`/`arm64`）。
    pub arch: String,
    /// 条目 ID（`26.04.1-amd64`；boot/seed URL 段）。
    pub id: String,
    /// 是否该架构最新稳定版（**滚动版本策略**：菜单/自动装机只认 latest；
    /// 老版本仅仓管理可见，供手动装机下载）。
    pub latest: bool,
    /// 字节数。
    pub size_bytes: u64,
    /// sha256（sidecar 对账/后台实算后回填；未知 = None）。
    pub sha256: Option<String>,
    /// sha256 状态：`sidecar`（对账记录在案）/`verified`（实算一致）/
    /// `mismatch`（实算不符）/`computing`。
    pub sha256_status: String,
    /// casper 提取状态：`done` / `extracting` / `pending` / `failed`。
    pub extraction_status: String,
    /// 提取件就绪明细（`vmlinuz`/`initrd` 实际存在的文件名列表）。
    pub extracted: Vec<String>,
    /// 失败/异常说明（无则 None）。
    pub error: Option<String>,
}

/// 同步扫描 isos 目录 → 状态列表（spawn_blocking 里跑；不计算 sha256、不提取，
/// 只登记 + 读 sidecar + 标记 latest）。文件名不合法（非仓形态）的文件跳过。
pub fn scan_netboot_repo_sync() -> Vec<IsoRepoFile> {
    let dir = netboot_isos_dir();
    let mut out: Vec<IsoRepoFile> = Vec::new();
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return out;
    };
    for ent in rd.flatten() {
        let Ok(meta) = ent.metadata() else { continue };
        if !meta.is_file() {
            continue;
        }
        let name = ent.file_name().to_string_lossy().to_string();
        let Some((version, arch, id)) = parse_repo_iso_filename(&name) else {
            continue; // 非仓形态（SHA256SUMS/sidecar/ipxe.iso…）不入 OS 清单
        };
        // sidecar 对账（存在即信——后台 verify 任务实算后回填/纠偏）
        let sidecar = dir.join(format!("{name}{ISO_SHA256_SIDECAR_SUFFIX}"));
        let (sha256, sha256_status) = std::fs::read_to_string(&sidecar)
            .ok()
            .and_then(|s| parse_sha256_line(&s))
            .map(|h| (Some(h), "sidecar".to_string()))
            .unwrap_or((None, "pending".to_string()));
        // 提取件就绪探测（boot/<id>/vmlinuz + initrd 实际存在性）
        let bdir = netboot_boot_dir(&id);
        let extracted: Vec<String> = ["vmlinuz", "initrd"]
            .iter()
            .filter(|f| bdir.join(f).is_file())
            .map(|f| f.to_string())
            .collect();
        let extraction_status = if extracted.len() == 2 {
            "done".to_string()
        } else {
            "pending".to_string()
        };
        out.push(IsoRepoFile {
            file: name,
            version,
            arch,
            id,
            latest: false,
            size_bytes: meta.len(),
            sha256,
            sha256_status,
            extraction_status,
            extracted,
            error: None,
        });
    }
    out.sort_by(|a, b| (&a.arch, &a.version).cmp(&(&b.arch, &b.version)));
    // 滚动版本策略：每架构只标最新稳定版
    for arch in NETBOOT_ARCHES {
        let best = out
            .iter()
            .filter(|f| f.arch == arch)
            .max_by(|a, b| netboot_version_cmp(&a.version, &b.version))
            .map(|f| f.file.clone());
        if let Some(file) = best {
            for f in out.iter_mut() {
                if f.file == file {
                    f.latest = true;
                }
            }
        }
    }
    out
}

/// 从 sidecar/SHA256SUMS 行解析首个 64 位 hex（`<hex>  <file>` 形态）。
fn parse_sha256_line(line: &str) -> Option<String> {
    let token = line.split_whitespace().next()?;
    (token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| token.to_ascii_lowercase())
}

/// 单件后台处理：sha256 实算对账（+写 sidecar）→ casper 提取落盘
/// `boot/<id>/{vmlinuz,initrd}`。返回更新后的状态项（供调用方回写内存态）。
fn verify_and_extract_iso_sync(file: &IsoRepoFile) -> IsoRepoFile {
    use std::io::Read as _;
    let mut st = file.clone();
    let path = netboot_iso_path(&st.file);
    // 1) sha256 实算（流式分块，不整读内存）
    let mut f = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            st.extraction_status = "failed".into();
            st.error = Some(format!("打开 ISO 失败: {e}"));
            return st;
        }
    };
    let mut hasher = Sha256::new();
    let mut size: u64 = 0;
    let mut buf = vec![0u8; NETBOOT_STREAM_CHUNK];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
                size += n as u64;
            }
            Err(e) => {
                st.extraction_status = "failed".into();
                st.error = Some(format!("读 ISO 失败: {e}"));
                return st;
            }
        }
    }
    let actual = format!("{:x}", hasher.finalize());
    st.size_bytes = size;
    if let Some(expect) = &st.sha256 {
        if expect != &actual {
            st.sha256_status = "mismatch".into();
            st.extraction_status = "failed".into();
            st.error = Some(format!(
                "sha256 不符: sidecar={expect} 实算={actual}（重新下载或修 sidecar）"
            ));
            return st;
        }
    }
    st.sha256 = Some(actual.clone());
    st.sha256_status = "verified".into();
    // sidecar 写回（下载对账记录入仓清单）
    let sidecar = netboot_isos_dir().join(format!("{}{ISO_SHA256_SIDECAR_SUFFIX}", st.file));
    let _ = std::fs::write(&sidecar, format!("{actual}  {}\n", st.file));
    // 2) casper 提取（内置 ISO9660 只读提取——免 7z/bsdtar/xorriso 装机依赖）
    let bdir = netboot_boot_dir(&st.id);
    if let Err(e) = std::fs::create_dir_all(&bdir) {
        st.extraction_status = "failed".into();
        st.error = Some(format!("创建提取目录失败: {e}"));
        return st;
    }
    for inner in ["casper/vmlinuz", "casper/initrd"] {
        let dest_name = inner.rsplit('/').next().unwrap_or(inner);
        let tmp = bdir.join(format!("{dest_name}.part"));
        let res = (|| -> Result<u64, String> {
            let mut iso = std::fs::File::open(&path).map_err(|e| format!("打开 ISO 失败: {e}"))?;
            let mut out =
                std::fs::File::create(&tmp).map_err(|e| format!("创建提取目标失败: {e}"))?;
            let n = iso9660_extract_file(&mut iso, inner, &mut out)?;
            std::fs::rename(&tmp, bdir.join(dest_name))
                .map_err(|e| format!("提取件落盘失败: {e}"))?;
            Ok(n)
        })();
        match res {
            Ok(n) if n > 0 => {}
            Ok(_) => {
                st.extraction_status = "failed".into();
                st.error = Some(format!("提取 {inner} 体积为 0"));
                let _ = std::fs::remove_file(&tmp);
                return st;
            }
            Err(e) => {
                st.extraction_status = "failed".into();
                st.error = Some(format!("提取 {inner} 失败: {e}"));
                let _ = std::fs::remove_file(&tmp);
                return st;
            }
        }
    }
    st.extracted = vec!["initrd".into(), "vmlinuz".into()];
    st.extraction_status = "done".into();
    st.error = None;
    st
}

// ----------------------------------------------------------------------------
// 内置 ISO9660 只读提取（os-provision::iso9660 的 os-api 自包含同款；
// 目录记录布局按真实 Ubuntu live-server ISO 校准：[1]=XAR 长度字节易漏）
// ----------------------------------------------------------------------------

const ISO_SECTOR: u64 = 2048;

/// 一条 ISO9660 目录记录的关键字段。
#[derive(Debug, Clone)]
struct IsoDirEntry {
    extent_lba: u64,
    data_len: u64,
    is_dir: bool,
    identifier: String,
}

impl IsoDirEntry {
    /// 标识符规范化：剥 `;版本`、转小写（`VMLINUZ.;1` ≡ `vmlinuz`）。
    fn norm_name(&self) -> String {
        let id = match self.identifier.find(';') {
            Some(i) => &self.identifier[..i],
            None => self.identifier.as_str(),
        };
        id.strip_suffix('.').unwrap_or(id).to_ascii_lowercase()
    }
}

/// 从 ISO（`Read+Seek`）提取 `inner_path`（如 `casper/vmlinuz`，大小写不敏感、
/// `/` 分层）到 `out`；返回字节数。路径段拒绝空/`.`/`..`（穿越防御）。
fn iso9660_extract_file<R: std::io::Read + std::io::Seek, W: std::io::Write>(
    iso: &mut R,
    inner_path: &str,
    out: &mut W,
) -> Result<u64, String> {
    let comps: Vec<String> = inner_path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
        .collect();
    if comps.is_empty() || comps.iter().any(|c| c == "." || c == "..") {
        return Err(format!("ISO 内路径非法: '{inner_path}'"));
    }
    let mut current = iso_root_entry(iso)?;
    for (i, comp) in comps.iter().enumerate() {
        let is_last = i == comps.len() - 1;
        let entries = iso_read_dir(iso, &current)?;
        let found = entries
            .iter()
            .find(|e| e.norm_name() == *comp)
            .ok_or_else(|| format!("ISO 内路径不存在: {inner_path}（段 '{comp}' 未命中）"))?;
        if !is_last && !found.is_dir {
            return Err(format!("ISO 内路径中段 '{comp}' 不是目录"));
        }
        if is_last && found.is_dir {
            return Err(format!("ISO 内路径 '{inner_path}' 是目录而非文件"));
        }
        current = found.clone();
    }
    // 数据区流式拷贝（256KiB 分块）
    iso.seek(std::io::SeekFrom::Start(current.extent_lba * ISO_SECTOR))
        .map_err(|e| format!("ISO seek 失败: {e}"))?;
    let mut remaining = current.data_len;
    let mut buf = vec![0u8; NETBOOT_STREAM_CHUNK];
    let mut copied: u64 = 0;
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        iso.read_exact(&mut buf[..want])
            .map_err(|e| format!("ISO read 失败: {e}"))?;
        out.write_all(&buf[..want])
            .map_err(|e| format!("写出提取件失败: {e}"))?;
        remaining -= want as u64;
        copied += want as u64;
    }
    Ok(copied)
}

/// 读 PVD（LBA 16 起扫卷描述符区）→ 根目录记录。
fn iso_root_entry<R: std::io::Read + std::io::Seek>(iso: &mut R) -> Result<IsoDirEntry, String> {
    let mut lba: u64 = 16;
    loop {
        if lba > 16 + 32 {
            return Err("未找到 ISO9660 主卷描述符（不是 ISO 镜像?）".to_string());
        }
        let mut sector = vec![0u8; ISO_SECTOR as usize];
        iso.seek(std::io::SeekFrom::Start(lba * ISO_SECTOR))
            .map_err(|e| format!("ISO seek 失败: {e}"))?;
        iso.read_exact(&mut sector)
            .map_err(|e| format!("ISO read 失败: {e}"))?;
        if &sector[1..6] != b"CD001" {
            return Err("卷描述符缺 CD001 标记（不是 ISO9660 镜像）".to_string());
        }
        if sector[0] == 255 {
            return Err("未找到主卷描述符（PVD）".to_string());
        }
        if sector[0] == 1 {
            return iso_parse_record(&sector[156..]);
        }
        lba += 1;
    }
}

/// 读一个目录 extent 的全部记录（跳过保留 id 0x00/0x01）。
fn iso_read_dir<R: std::io::Read + std::io::Seek>(
    iso: &mut R,
    dir: &IsoDirEntry,
) -> Result<Vec<IsoDirEntry>, String> {
    let mut raw = vec![0u8; dir.data_len as usize];
    iso.seek(std::io::SeekFrom::Start(dir.extent_lba * ISO_SECTOR))
        .map_err(|e| format!("ISO seek 失败: {e}"))?;
    iso.read_exact(&mut raw)
        .map_err(|e| format!("ISO read 失败: {e}"))?;
    let mut out = Vec::new();
    let mut off = 0usize;
    while off < raw.len() {
        let rec_len = raw[off] as usize;
        if rec_len == 0 {
            // 记录不跨扇区：跳到下一扇区边界
            let next = (off / ISO_SECTOR as usize + 1) * ISO_SECTOR as usize;
            if next >= raw.len() {
                break;
            }
            off = next;
            continue;
        }
        if off + rec_len > raw.len() {
            return Err("目录记录越界（ISO 损坏?）".to_string());
        }
        let e = iso_parse_record(&raw[off..off + rec_len])?;
        if e.identifier != "\0" && e.identifier != "\u{1}" {
            out.push(e);
        }
        off += rec_len;
    }
    Ok(out)
}

/// 解析一条目录记录（ECMA-119：`[0]`长 `[1]`XAR 长 `[2:6]`extent LE
/// `[10:14]`数据长 LE `[25]`flags `[32]`id 长 `[33..]`id）。
fn iso_parse_record(rec: &[u8]) -> Result<IsoDirEntry, String> {
    let len = *rec.first().ok_or("空目录记录")? as usize;
    if len < 34 || len > rec.len() {
        return Err(format!("目录记录长度非法: {len}"));
    }
    let extent_lba = u32::from_le_bytes([rec[2], rec[3], rec[4], rec[5]]) as u64;
    let data_len = u32::from_le_bytes([rec[10], rec[11], rec[12], rec[13]]) as u64;
    let flags = rec[25];
    let id_len = rec[32] as usize;
    if 33 + id_len > len {
        return Err("标识符长度越界".to_string());
    }
    Ok(IsoDirEntry {
        extent_lba,
        data_len,
        is_dir: flags & 0x02 != 0,
        identifier: String::from_utf8_lossy(&rec[33..33 + id_len]).to_string(),
    })
}

// ----------------------------------------------------------------------------
// 网络装机：HTTP Range 解析（纯函数）+ tokio::fs 流式直传 handler
// ----------------------------------------------------------------------------

/// 单区间 Range（多区间按 RFC 7233 可整体忽略 → 回退 Full）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpRange {
    /// 无/忽略 Range：整文件 200。
    Full,
    /// 满足：`bytes=start-(start+len-1)` → 206。
    Partial { start: u64, len: u64 },
}

/// 解析 `Range: bytes=...` 头（单区间 `a-b`/`a-`/`-suffix`；其余形态忽略回
/// Full——多区间/语法坏不 416，合法宽松策略）。`Err(())` = 区间不可满足 → 416。
#[must_use]
pub fn parse_range_header(header: Option<&str>, size: u64) -> Result<HttpRange, ()> {
    let Some(h) = header else {
        return Ok(HttpRange::Full);
    };
    let h = h.trim();
    let Some(spec) = h.strip_prefix("bytes=") else {
        return Ok(HttpRange::Full);
    };
    if spec.contains(',') {
        return Ok(HttpRange::Full); // 多区间：忽略（合法）
    }
    let spec = spec.trim();
    if let Some(suffix) = spec.strip_prefix('-') {
        // bytes=-N（末 N 字节）
        let n: u64 = suffix.trim().parse().map_err(|_| ())?;
        if n == 0 || size == 0 {
            return Err(());
        }
        let len = n.min(size);
        return Ok(HttpRange::Partial {
            start: size - len,
            len,
        });
    }
    let (a, b) = spec.split_once('-').ok_or(())?;
    let start: u64 = a.trim().parse().map_err(|_| ())?;
    if start >= size {
        return Err(()); // 起点越界 → 416
    }
    let len = match b.trim() {
        "" => size - start, // bytes=a- 到尾
        end => {
            let end: u64 = end.parse().map_err(|_| ())?;
            let end = end.min(size - 1).max(start); // clamp 到文件内
            end - start + 1
        }
    };
    if len == 0 {
        return Err(());
    }
    Ok(HttpRange::Partial { start, len })
}

/// 仓白名单解析结果（流式 handler 的前置判定）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetbootFileStream {
    /// OS ISO（`isos/<file>`，形态白名单过）。
    Iso(PathBuf),
    /// 引导介质（`isos/ipxe.iso` 等，精确名白名单过）。
    BootMedia(PathBuf),
    /// casper 提取件（`boot/<id>/{vmlinuz,initrd}`，id 白名单过）。
    Casper(PathBuf),
}

/// 流式直传目标解析：**白名单核心**——OS ISO 走 [`parse_repo_iso_filename`]
/// 精确形态；提取件 id 必须回构出合法仓文件名（严格点分版本 + 受支持架构）
/// 且文件名 ∈ {vmlinuz, initrd}；引导介质走精确名常量表。任何路径注入形态
/// （`../`、`\`、编码、绝对路径…）都无法命中——不存在用户输入的路径拼接。
#[must_use]
pub fn resolve_netboot_stream(
    iso_file: Option<&str>,
    boot: Option<(&str, &str)>,
) -> Option<NetbootFileStream> {
    match (iso_file, boot) {
        (Some(file), None) => {
            if parse_repo_iso_filename(file).is_some() {
                return Some(NetbootFileStream::Iso(netboot_iso_path(file)));
            }
            netboot_boot_media_path(file).map(NetbootFileStream::BootMedia)
        }
        (None, Some((id, file))) => {
            // id = `<点分版本>-<amd64|arm64>`（两段都不含 '-'，末段拆分安全）；
            // 回构完整仓文件名过形态白名单，canonical id 必须与入参精确一致
            let Some((version, arch)) = id.rsplit_once('-') else {
                return None;
            };
            let Some((_, _, canonical_id)) =
                parse_repo_iso_filename(&format!("ubuntu-{version}-live-server-{arch}.iso"))
            else {
                return None;
            };
            if canonical_id != id || !matches!(file, "vmlinuz" | "initrd") {
                return None;
            }
            Some(NetbootFileStream::Casper(netboot_boot_dir(id).join(file)))
        }
        _ => None,
    }
}

/// tokio::fs 流式直传响应（**不进 base64 网关 body**，`direct_passthrough_bytes`
/// 的流式扩展先例）：`tokio::fs::File` + `futures` 分块流 + `accept-ranges:
/// bytes` + Range 206/416 + `x-nexos-sha256`（sidecar 已知时附带）。
async fn netboot_stream_response(
    target: Option<NetbootFileStream>,
    range_header: Option<&str>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use tokio::io::{AsyncReadExt as _, AsyncSeekExt as _};

    let Some(target) = target else {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({
                "error": "未知仓文件（白名单外的名字/路径形态被拒绝）"
            })),
        )
            .into_response();
    };
    let path = match &target {
        NetbootFileStream::Iso(p)
        | NetbootFileStream::BootMedia(p)
        | NetbootFileStream::Casper(p) => p,
    };
    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(serde_json::json!({
                    "error": format!("文件不存在或不可读（先 POST /api/v1/provisioning/isos/scan 登记/提取: {}）", path.display())
                })),
            )
                .into_response();
        }
    };
    let size = match file.metadata().await {
        Ok(m) if m.is_file() => m.len(),
        _ => return (StatusCode::NOT_FOUND, "not a regular file").into_response(),
    };
    let range = match parse_range_header(range_header, size) {
        Ok(r) => r,
        Err(()) => {
            // 416 + Content-Range: bytes */<size>（RFC 7233）
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [("content-range", format!("bytes */{size}"))],
                axum::Json(serde_json::json!({"error": format!("Range 不可满足（size={size}）")})),
            )
                .into_response();
        }
    };
    let (start, len, status) = match range {
        HttpRange::Full => (0u64, size, StatusCode::OK),
        HttpRange::Partial { start, len } => (start, len, StatusCode::PARTIAL_CONTENT),
    };

    let mut file = file;
    if let Err(e) = file.seek(std::io::SeekFrom::Start(start)).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({"error": format!("seek 失败: {e}")})),
        )
            .into_response();
    }

    // 流：每次读 ≤256KiB、共 len 字节（ReaderStream 同款行为，免 tokio-util 依赖）
    let stream = futures::stream::unfold((file, len), |(mut f, remaining)| async move {
        if remaining == 0 {
            return None;
        }
        let want = (remaining as usize).min(NETBOOT_STREAM_CHUNK);
        let mut buf = vec![0u8; want];
        match f.read_exact(&mut buf).await {
            Ok(n) => {
                buf.truncate(n);
                let rem = remaining - n as u64;
                Some((
                    Ok::<_, std::io::Error>(axum::body::Bytes::from(buf)),
                    (f, rem),
                ))
            }
            Err(e) => Some((Err(e), (f, 0))),
        }
    });

    let mut builder = axum::response::Response::builder()
        .status(status)
        .header("content-type", "application/octet-stream")
        .header("accept-ranges", "bytes")
        .header("content-length", len.to_string());
    if let HttpRange::Partial { start, len } = range {
        builder = builder.header(
            "content-range",
            format!("bytes {start}-{}/{}", start + len - 1, size),
        );
    }
    // sha256 头（sidecar 已知时；OS ISO/引导介质均适用——Casper 提取件无 sidecar）
    if matches!(
        target,
        NetbootFileStream::Iso(_) | NetbootFileStream::BootMedia(_)
    ) {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let sidecar = netboot_isos_dir().join(format!("{name}{ISO_SHA256_SIDECAR_SUFFIX}"));
        if let Some(hex) = std::fs::read_to_string(&sidecar)
            .ok()
            .and_then(|s| parse_sha256_line(&s))
        {
            builder = builder.header("x-nexos-sha256", hex);
        }
    }
    builder
        .body(axum::body::Body::from_stream(stream))
        .unwrap_or_else(|_| {
            (StatusCode::INTERNAL_SERVER_ERROR, "body build failed").into_response()
        })
}

/// axum 特挂路由 handler：`GET /api/v1/provisioning/isos/{file}`（流式直传，
/// Range 206/416；白名单见 [`resolve_netboot_stream`]）。不经网关 JSON 信封。
pub async fn netboot_iso_stream_handler(
    axum::extract::Path(file): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let range = headers
        .get(axum::http::header::RANGE)
        .and_then(|v| v.to_str().ok());
    netboot_stream_response(resolve_netboot_stream(Some(&file), None), range).await
}

/// axum 特挂路由 handler：`GET /api/v1/provisioning/boot/{id}/{file}`（casper
/// vmlinuz/initrd 流式直传；同上白名单与 Range 语义）。
pub async fn netboot_boot_stream_handler(
    axum::extract::Path((id, file)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    let range = headers
        .get(axum::http::header::RANGE)
        .and_then(|v| v.to_str().ok());
    netboot_stream_response(resolve_netboot_stream(None, Some((&id, &file))), range).await
}

// ----------------------------------------------------------------------------
// 子进程执行辅助（ssh / scp / 通用）
// ----------------------------------------------------------------------------

/// 一次子进程执行的结果（含超时标记）。
#[derive(Debug, Clone)]
struct ProcOutcome {
    exit_code: i32,
    stdout: String,
    stderr: String,
    timed_out: bool,
}

impl ProcOutcome {
    /// 是否成功（退出码 0 且未超时）。
    fn is_success(&self) -> bool {
        self.exit_code == 0 && !self.timed_out
    }

    /// 失败摘要（供任务 error 字段）。
    fn err_summary(&self) -> String {
        if self.timed_out {
            return format!("超时被终止（{}）", self.stderr);
        }
        if self.stderr.trim().is_empty() {
            format!("exit_code={}", self.exit_code)
        } else {
            format!("exit_code={}: {}", self.exit_code, self.stderr.trim())
        }
    }
}

/// 截断输出到 `cap` 字节（UTF-8 边界安全），超长附标记。
fn truncate_output(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[截断，原 {} 字节]", &s[..end], s.len())
}

/// 单引号包裹的 POSIX shell 引用（`'` → `'"'"'`），用于拼远端命令串。
///
/// 我们本地 spawn 不经 shell（argv 直传），此引用只影响远端 `sh` 的解析，
/// 保证路径/命令中的空格与元字符不被拆分。
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// 目标实际使用的私钥路径（未配置时用默认）。
fn ssh_key_of(target: &SshTarget) -> String {
    target
        .private_key_path
        .clone()
        .unwrap_or_else(|| DEFAULT_SSH_KEY.to_string())
}

/// 带超时执行子进程：超时由 `kill_on_drop` 强杀（future 被 drop 即 kill）。
async fn run_timed(mut cmd: tokio::process::Command, timeout: Duration) -> ProcOutcome {
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => ProcOutcome {
            exit_code: out.status.code().unwrap_or(-1),
            stdout: truncate_output(&String::from_utf8_lossy(&out.stdout), DEPLOY_OUTPUT_CAP),
            stderr: truncate_output(&String::from_utf8_lossy(&out.stderr), DEPLOY_OUTPUT_CAP),
            timed_out: false,
        },
        Ok(Err(e)) => ProcOutcome {
            exit_code: -1,
            stdout: String::new(),
            stderr: format!("spawn 失败: {e}"),
            timed_out: false,
        },
        Err(_) => ProcOutcome {
            exit_code: -1,
            stdout: String::new(),
            stderr: format!("超时（{}s）被终止", timeout.as_secs()),
            timed_out: true,
        },
    }
}

/// `ssh <opts> -p <port> -i <key> user@host '<remote_cmd>'`（remote_cmd 单 argv 直传）。
async fn run_ssh(target: &SshTarget, remote_cmd: &str, timeout: Duration) -> ProcOutcome {
    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args(SSH_BASE_OPTS)
        .args(["-p", &target.port.to_string()])
        .args(["-i", &ssh_key_of(target)])
        .arg(format!("{}@{}", target.user, target.host))
        .arg(remote_cmd);
    run_timed(cmd, timeout).await
}

/// `scp <opts> -P <port> -i <key> local user@host:remote`（注意 scp 大写 -P）。
async fn run_scp(target: &SshTarget, local: &str, remote: &str, timeout: Duration) -> ProcOutcome {
    let mut cmd = tokio::process::Command::new("scp");
    cmd.args(SSH_BASE_OPTS)
        .args(["-P", &target.port.to_string()])
        .args(["-i", &ssh_key_of(target)])
        .arg(local)
        .arg(format!("{}@{}:{}", target.user, target.host, remote));
    run_timed(cmd, timeout).await
}

/// 远端路径的父目录（无 `/` 的裸文件名返回 None）。
fn remote_parent_dir(remote_path: &str) -> Option<String> {
    match remote_path.rfind('/') {
        Some(0) => None, // 根下的文件（/a → 父目录 "/"，无需 mkdir）
        Some(idx) => Some(remote_path[..idx].to_string()),
        None => None,
    }
}

// ----------------------------------------------------------------------------
// LoggingIsoRunner —— os-iso 子进程执行的日志包装
// ----------------------------------------------------------------------------

/// 一个进行中（或刚结束）ISO 构建的观测句柄。
#[derive(Debug, Clone)]
struct IsoBuildHandle {
    /// 逐行日志（命令 + 退出码 + 输出摘要）。
    log: Arc<Mutex<Vec<String>>>,
    /// 当前步骤（程序名）。
    step: Arc<Mutex<String>>,
    /// 估计进度 0.0~1.0。
    progress: Arc<Mutex<f32>>,
}

impl IsoBuildHandle {
    fn new() -> Self {
        Self {
            log: Arc::new(Mutex::new(Vec::new())),
            step: Arc::new(Mutex::new("prepare".to_string())),
            progress: Arc::new(Mutex::new(0.0)),
        }
    }

    fn push_log(&self, line: String) {
        let mut log = self.log.lock().expect("iso log poisoned");
        log.push(format!("{} {}", now_iso(), line));
        if log.len() > ISO_LOG_MAX_LINES {
            let overflow = log.len() - ISO_LOG_MAX_LINES;
            log.drain(0..overflow);
        }
    }
}

/// `IsoBuildRunner` 实现：真实 spawn 子进程 + 每步记录日志 + 关键产物目录预创建。
///
/// os-iso 的 `TokioIsoRunner` 不暴露过程日志，本包装在 os-api 侧补齐
/// （不改动 os-iso pub API）：每次 `run` 前记录完整命令行，后记录退出码与
/// stdout/stderr 摘要；并对 mksquashfs 的源/输出目录与 xorriso 的 `-o` 产物
/// 目录做 `create_dir_all` 预创建（os-iso 只派生路径不建目录）。
struct LoggingIsoRunner {
    handle: IsoBuildHandle,
}

impl LoggingIsoRunner {
    fn new(handle: IsoBuildHandle) -> Self {
        Self { handle }
    }

    /// 预创建子进程需要的目录（best effort，失败仅记日志）。
    fn ensure_dirs(&self, program: &str, args: &[String]) {
        let mut dirs: Vec<String> = Vec::new();
        match program {
            // mksquashfs <source_dir> <output_file> ...
            "mksquashfs" if args.len() >= 2 => {
                dirs.push(args[0].clone());
                if let Some(parent) = PathBuf::from(&args[1]).parent() {
                    dirs.push(parent.to_string_lossy().into_owned());
                }
            }
            // xorriso ... -o <iso> <source_tree>
            "xorriso" => {
                if let Some(pos) = args.iter().position(|a| a == "-o") {
                    if let Some(out) = args.get(pos + 1) {
                        if let Some(parent) = PathBuf::from(out).parent() {
                            dirs.push(parent.to_string_lossy().into_owned());
                        }
                    }
                }
                if let Some(tree) = args.last() {
                    dirs.push(tree.clone());
                }
            }
            _ => {}
        }
        for d in dirs {
            if !d.is_empty() {
                match std::fs::create_dir_all(&d) {
                    Ok(()) => self.handle.push_log(format!("mkdir -p {d}")),
                    Err(e) => self.handle.push_log(format!("mkdir -p {d} 失败: {e}")),
                }
            }
        }
    }
}

#[async_trait]
impl IsoBuildRunner for LoggingIsoRunner {
    async fn run(&self, program: &str, args: &[String]) -> Result<ProcessOutput, IsoError> {
        self.ensure_dirs(program, args);
        *self.handle.step.lock().expect("iso step poisoned") = program.to_string();
        self.handle
            .push_log(format!("$ {program} {}", args.join(" ")));

        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);
        let out = cmd
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .map_err(|e| IsoError::BuildFailed(format!("spawn {program} 失败: {e}")))?;

        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let exit_code = out.status.code().unwrap_or(-1);
        if exit_code != 0 || !stdout.is_empty() || !stderr.is_empty() {
            self.handle.push_log(format!(
                "{program} exit={exit_code} stdout=[{}] stderr=[{}]",
                truncate_output(stdout.trim(), 2048),
                truncate_output(stderr.trim(), 2048),
            ));
        } else {
            self.handle.push_log(format!("{program} exit=0"));
        }

        // 步骤完成度推进（squashfs→0.4 / xorriso→0.8 / sha256→0.95）
        let done = match program {
            "mksquashfs" => 0.4f32,
            "xorriso" => 0.8f32,
            _ => 0.95f32,
        };
        *self.handle.progress.lock().expect("iso progress poisoned") = done;

        Ok(ProcessOutput {
            stdout,
            stderr,
            exit_code,
        })
    }
}

// ----------------------------------------------------------------------------
// ProvisioningRouteHandler
// ----------------------------------------------------------------------------

/// 系统自举路由处理器——HTTP 边界适配到内存态 PXE/ISO/SSH 编排 + 真实子进程执行。
pub struct ProvisioningRouteHandler {
    pxe_config: Mutex<PxeConfig>,
    boot_entries: Mutex<Vec<BootEntry>>,
    pxe_running: Mutex<bool>,
    /// Arc 供 `tokio::spawn` 的构建收尾任务回写状态。
    iso_tasks: Arc<Mutex<Vec<IsoTask>>>,
    ssh_targets: Mutex<Vec<SshTarget>>,
    /// Arc 同上（部署后台任务回写）。
    deploy_tasks: Arc<Mutex<Vec<DeployTask>>>,
    /// target_id → 进行中的 deploy_id（同目标互斥）。
    busy_deploys: Arc<Mutex<HashMap<String, String>>>,
    /// iso 任务 id → 构建观测句柄（building 期间存在）。
    iso_builds: Arc<Mutex<HashMap<String, IsoBuildHandle>>>,
    /// 网络装机 ISO 仓登记态（scan 登记 + 后台 sha256/提取任务回写；目录真身
    /// 在 `<NEXOS_PROVISION_REPO>/isos`，内存态只是观测缓存）。
    netboot_repo: Arc<Mutex<Vec<IsoRepoFile>>>,
    /// ISO 产物输出根目录（env NEXOS_ISO_OUT 可覆盖）。
    iso_output_root: PathBuf,
    deploy_file_timeout: Duration,
    deploy_cmd_timeout: Duration,
    counter: Mutex<u64>,
    /// 共享的 update 组件实例（装配层 Arc 双持）：prepare-distributable
    /// 成功后自动登记同版本更新工件（页内 apply 通道）。None = 未注入
    /// （单测构造）——prepare 只暂存分发件，不登记。
    update_registry: Option<Arc<UpdateRouteHandler>>,
    /// 统一任务登记表（v0.1.54 双写旁路；None=未注入，iso/deploy 行为与
    /// 历史完全一致）。iso_tasks/deploy_tasks 内存态**保留不动**——重启
    /// 即丢的现状不变，重启可见性由 unified_tasks 兜底。
    unified: Option<Arc<UnifiedTaskStore>>,
}

impl ProvisioningRouteHandler {
    /// 构造 handler，预置示例数据（PXE config + 2 boot entries + 1 completed ISO
    /// + 1 SSH target）。ISO 产物目录取 env `NEXOS_ISO_OUT`（默认 `./build/iso`）。
    #[must_use]
    pub fn new() -> Self {
        let iso_output_root = std::env::var_os("NEXOS_ISO_OUT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("./build/iso"));
        Self::with_options(iso_output_root, DEPLOY_FILE_TIMEOUT, DEPLOY_CMD_TIMEOUT)
    }

    /// 用空状态构造（测试注入干净场景）。
    #[must_use]
    pub fn with_empty() -> Self {
        let mut h = Self::new();
        h.pxe_config = Mutex::new(PxeConfig {
            enabled: false,
            tftp_server: String::new(),
            boot_mode: "uefi".into(),
            http_repo: String::new(),
            default_bootfile: String::new(),
        });
        h.boot_entries = Mutex::new(Vec::new());
        h.pxe_running = Mutex::new(false);
        h.iso_tasks = Arc::new(Mutex::new(Vec::new()));
        h.ssh_targets = Mutex::new(Vec::new());
        h.deploy_tasks = Arc::new(Mutex::new(Vec::new()));
        h
    }

    /// 全参构造（测试注入超时与 ISO 产物目录）。
    #[must_use]
    pub fn with_options(
        iso_output_root: PathBuf,
        deploy_file_timeout: Duration,
        deploy_cmd_timeout: Duration,
    ) -> Self {
        Self {
            pxe_config: Mutex::new(default_pxe_config()),
            boot_entries: Mutex::new(default_boot_entries()),
            pxe_running: Mutex::new(false),
            iso_tasks: Arc::new(Mutex::new(demo_iso_tasks())),
            ssh_targets: Mutex::new(demo_ssh_targets()),
            deploy_tasks: Arc::new(Mutex::new(Vec::new())),
            busy_deploys: Arc::new(Mutex::new(HashMap::new())),
            iso_builds: Arc::new(Mutex::new(HashMap::new())),
            netboot_repo: Arc::new(Mutex::new(Vec::new())),
            iso_output_root,
            deploy_file_timeout,
            deploy_cmd_timeout,
            counter: Mutex::new(100),
            update_registry: None,
            unified: None,
        }
    }

    /// 注入统一任务登记表（v0.1.54 统一任务框架：iso/deploy 任务创建/终态
    /// 双写 `unified_tasks`；轻旁路——登记失败仅日志）。
    #[must_use]
    pub fn with_unified_tasks(mut self, store: Arc<UnifiedTaskStore>) -> Self {
        self.unified = Some(store);
        self
    }

    /// 统一登记小工具：iso 任务登记（创建 queued / 重建重放）。
    fn unified_register_iso(&self, task: &IsoTask) {
        if let Some(store) = &self.unified {
            store.register(&UnifiedTask {
                id: task.id.clone(),
                kind: "iso".into(),
                status: UnifiedStatus::from_source("iso", &task.status),
                label: format!(
                    "ISO 构建 {}（{} {} {}）",
                    task.name, task.variant, task.arch, task.ubuntu_version
                ),
                stage: None,
                log_tail: String::new(),
                output: None,
                created_at: task.created_at.clone(),
                finished_at: None,
                error: None,
            });
        }
    }

    /// 统一登记小工具：终态落表（iso/deploy 共用；status 取源串自动归一）。
    fn unified_finish(
        unified: &Option<Arc<UnifiedTaskStore>>,
        kind: &str,
        id: &str,
        source_status: &str,
        label: &str,
        output: Option<&str>,
        error: Option<&str>,
    ) {
        if let Some(store) = unified {
            store.finish(
                kind,
                id,
                UnifiedStatus::from_source(kind, source_status),
                label,
                output,
                error,
            );
        }
    }

    /// 注入共享的 update 组件实例（与 update 组件注册的**同一实例**——
    /// Arc 双持，见 main.rs 装配）：prepare-distributable 成功后把同版本
    /// 工件登记进其工件表（version=运行二进制 `CARGO_PKG_VERSION`、
    /// path=分发产物，复用 POST /update/artifact 的全部校验 + sha256，
    /// 重复 version 覆盖）。发版流程"三节点各跑一次 prepare"即同时喂饱
    /// dist 下载与页内 apply 两条更新通道。未注入（全部单测构造）时
    /// prepare 只暂存分发件，行为与历史版本一致。
    #[must_use]
    pub fn with_update_registry(mut self, registry: Arc<UpdateRouteHandler>) -> Self {
        self.update_registry = Some(registry);
        self
    }

    /// 当前 PXE 配置快照（测试 / 诊断用）。
    #[must_use]
    pub fn pxe_config_snapshot(&self) -> PxeConfig {
        self.pxe_config.lock().expect("pxe_config poisoned").clone()
    }

    /// 当前启动条目列表快照。
    #[must_use]
    pub fn boot_entries_snapshot(&self) -> Vec<BootEntry> {
        self.boot_entries
            .lock()
            .expect("boot_entries poisoned")
            .clone()
    }

    /// 当前 PXE 服务运行态快照。
    #[must_use]
    pub fn pxe_running_snapshot(&self) -> bool {
        *self.pxe_running.lock().expect("pxe_running poisoned")
    }

    /// 当前 ISO 任务列表快照。
    #[must_use]
    pub fn iso_tasks_snapshot(&self) -> Vec<IsoTask> {
        self.iso_tasks.lock().expect("iso_tasks poisoned").clone()
    }

    /// 当前 SSH 目标列表快照。
    #[must_use]
    pub fn ssh_targets_snapshot(&self) -> Vec<SshTarget> {
        self.ssh_targets
            .lock()
            .expect("ssh_targets poisoned")
            .clone()
    }

    /// 当前部署任务列表快照。
    #[must_use]
    pub fn deploy_tasks_snapshot(&self) -> Vec<DeployTask> {
        self.deploy_tasks
            .lock()
            .expect("deploy_tasks poisoned")
            .clone()
    }

    /// 生成下一个 id（prefix-N）。
    fn next_id(&self, prefix: &str) -> String {
        let mut c = self.counter.lock().expect("counter poisoned");
        *c += 1;
        format!("{prefix}-{}", *c)
    }

    /// ISO 仓登记态快照（未 scan 过时现场扫一次目录——只读 sidecar 不做重活）。
    fn netboot_repo_snapshot(&self) -> Vec<IsoRepoFile> {
        let mut repo = self.netboot_repo.lock().expect("netboot_repo poisoned");
        if repo.is_empty() {
            *repo = scan_netboot_repo_sync();
        }
        repo.clone()
    }

    /// 扫描登记 + 对每件未就绪 ISO 起**后台任务**：sha256 实算对账（+写 sidecar）
    /// → casper 提取落盘 `boot/<id>/`。立即返回登记结果（状态 pending/computing
    /// 可由 `GET /isos` 轮询观测）。
    fn netboot_scan_and_process(&self) -> Vec<IsoRepoFile> {
        let scanned = scan_netboot_repo_sync();
        let repo = Arc::clone(&self.netboot_repo);
        let mut by_file: HashMap<String, IsoRepoFile> = scanned
            .iter()
            .cloned()
            .map(|f| (f.file.clone(), f))
            .collect();
        // 融合既有登记态（保留后台计算出的 sha256 状态）
        if let Ok(existing) = repo.lock() {
            for old in existing.iter() {
                if let Some(new) = by_file.get_mut(&old.file) {
                    if old.sha256_status == "verified" && new.sha256.is_none() {
                        new.sha256 = old.sha256.clone();
                        new.sha256_status = "verified".into();
                    }
                }
            }
        }
        let merged: Vec<IsoRepoFile> = by_file.into_values().collect();
        *repo.lock().expect("netboot_repo poisoned") = merged.clone();
        // 每件未完成件起后台任务（spawn_blocking：sha256 3GB 级实算不能占 runtime）
        for st in merged.clone() {
            if st.extraction_status == "done"
                && matches!(st.sha256_status.as_str(), "verified" | "sidecar")
            {
                continue;
            }
            let repo = Arc::clone(&repo);
            let file = st.file.clone();
            let mut target = st.clone();
            target.sha256_status = "computing".into();
            target.extraction_status = if target.extraction_status == "done" {
                "done".into()
            } else {
                "extracting".into()
            };
            if let Ok(mut r) = repo.lock() {
                if let Some(slot) = r.iter_mut().find(|f| f.file == file) {
                    *slot = target.clone();
                }
            }
            tokio::spawn(async move {
                let file = file.clone();
                let repo = Arc::clone(&repo);
                let done =
                    tokio::task::spawn_blocking(move || verify_and_extract_iso_sync(&target)).await;
                match done {
                    Ok(updated) => {
                        if let Ok(mut r) = repo.lock() {
                            if let Some(slot) = r.iter_mut().find(|f| f.file == file) {
                                *slot = updated;
                            }
                        }
                    }
                    Err(e) => {
                        if let Ok(mut r) = repo.lock() {
                            if let Some(slot) = r.iter_mut().find(|f| f.file == file) {
                                slot.extraction_status = "failed".into();
                                slot.error = Some(format!("后台任务 join 失败: {e}"));
                            }
                        }
                    }
                }
            });
        }
        merged
    }

    /// 统计快照。
    fn stats_snapshot(&self) -> ProvisioningStats {
        let pxe_running = *self.pxe_running.lock().expect("pxe_running poisoned");
        let iso_tasks = self.iso_tasks.lock().expect("iso_tasks poisoned");
        let ssh_targets = self.ssh_targets.lock().expect("ssh_targets poisoned");
        let deploys_total = self
            .deploy_tasks
            .lock()
            .expect("deploy_tasks poisoned")
            .len();
        ProvisioningStats {
            pxe_running,
            iso_tasks_total: iso_tasks.len(),
            iso_completed: iso_tasks.iter().filter(|t| t.status == "completed").count(),
            iso_failed: iso_tasks.iter().filter(|t| t.status == "failed").count(),
            ssh_targets_total: ssh_targets.len(),
            ssh_reachable: ssh_targets
                .iter()
                .filter(|t| t.status == "reachable")
                .count(),
            deploys_total,
        }
    }

    /// 真实测试 SSH 连接（调系统 ssh 子进程，BatchMode=yes 禁密码交互）。
    ///
    /// stdout 含 "os-ok" → reachable；否则 unreachable。整体 15s 超时
    /// （ConnectTimeout 只管 TCP 建连，命令挂死也兜得住）。ssh 不存在 /
    /// 任何失败均返回 unreachable，不 panic 不报错。
    async fn test_ssh_connection(target: &SshTarget) -> bool {
        let out = run_ssh(target, "echo os-ok", SSH_TEST_TIMEOUT).await;
        out.is_success() && out.stdout.contains("os-ok")
    }

    /// 就地更新一个部署任务（后台执行器逐步回写）。
    fn patch_deploy(
        tasks: &Arc<Mutex<Vec<DeployTask>>>,
        id: &str,
        f: impl FnOnce(&mut DeployTask),
    ) {
        let mut guard = tasks.lock().expect("deploy_tasks poisoned");
        if let Some(t) = guard.iter_mut().find(|t| t.id == id) {
            f(t);
        }
    }

    /// 部署终态落库 + 释放同目标互斥（所有结束路径必经）+ 统一登记终态。
    #[allow(clippy::too_many_arguments)] // 旁路登记注入两参（api_gateway 同款豁免）
    fn finish_deploy(
        tasks: &Arc<Mutex<Vec<DeployTask>>>,
        busy: &Arc<Mutex<HashMap<String, String>>>,
        unified: &Option<Arc<UnifiedTaskStore>>,
        label: &str,
        id: &str,
        target_id: &str,
        status: &str,
        error: Option<String>,
    ) {
        Self::patch_deploy(tasks, id, |t| {
            t.status = status.to_string();
            t.error = error.clone();
            t.finished_at = Some(now_iso());
        });
        busy.lock()
            .expect("busy_deploys poisoned")
            .remove(target_id);
        Self::unified_finish(unified, "deploy", id, status, label, None, error.as_deref());
    }

    /// 后台执行一次部署（在 `tokio::spawn` 中跑）：
    /// mkdir 远端目录 → 逐文件 scp → 可选 run_cmd → 终态回写。
    async fn run_deploy(
        tasks: Arc<Mutex<Vec<DeployTask>>>,
        busy: Arc<Mutex<HashMap<String, String>>>,
        target: SshTarget,
        task_id: String,
        file_timeout: Duration,
        cmd_timeout: Duration,
        unified: Option<Arc<UnifiedTaskStore>>,
    ) {
        let label = format!("SSH 部署 → {}（{}@{}）", target.name, target.user, target.host);
        let (files, run_cmd) = {
            let guard = tasks.lock().expect("deploy_tasks poisoned");
            let t = match guard.iter().find(|t| t.id == task_id) {
                Some(t) => t,
                None => return, // 任务被删除：直接放弃
            };
            (t.files.clone(), t.run_cmd.clone())
        };

        Self::patch_deploy(&tasks, &task_id, |t| {
            t.status = if files.is_empty() {
                "running".into()
            } else {
                "transferring".into()
            };
            t.started_at = Some(now_iso());
        });
        if let Some(store) = &unified {
            store.progress(
                "deploy",
                &task_id,
                Some(if files.is_empty() { "running" } else { "transferring" }),
                "",
            );
        }

        // —— 阶段一：远端目录预创建（去重后一次 mkdir -p）——
        if !files.is_empty() {
            let mut dirs: Vec<String> = files
                .iter()
                .filter_map(|f| remote_parent_dir(&f.remote_path))
                .collect();
            dirs.sort();
            dirs.dedup();
            if !dirs.is_empty() {
                let remote = format!(
                    "mkdir -p {}",
                    dirs.iter()
                        .map(|d| sh_quote(d))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                let out = run_ssh(&target, &remote, cmd_timeout).await;
                if !out.is_success() {
                    let err = format!("远程目录创建失败: {}", out.err_summary());
                    Self::patch_deploy(&tasks, &task_id, |t| {
                        for r in t.results.iter_mut() {
                            if r.status == "pending" {
                                r.status = "failed".into();
                                r.error = Some("远程目录创建失败，未尝试传输".into());
                            }
                        }
                    });
                    Self::finish_deploy(&tasks, &busy, &unified, &label, &task_id, &target.id, "failed", Some(err));
                    return;
                }
            }
        }

        // —— 阶段二：逐文件 scp ——
        for (idx, f) in files.iter().enumerate() {
            let started = Instant::now();
            let out = run_scp(&target, &f.local_path, &f.remote_path, file_timeout).await;
            let duration = started.elapsed().as_millis() as u64;
            if out.is_success() {
                Self::patch_deploy(&tasks, &task_id, |t| {
                    if let Some(r) = t.results.get_mut(idx) {
                        r.status = "success".into();
                        r.exit_code = Some(0);
                        r.duration_ms = Some(duration);
                        r.error = None;
                    }
                });
            } else {
                let summary = out.err_summary();
                Self::patch_deploy(&tasks, &task_id, |t| {
                    if let Some(r) = t.results.get_mut(idx) {
                        r.status = "failed".into();
                        r.exit_code = Some(out.exit_code);
                        r.duration_ms = Some(duration);
                        r.error = Some(summary.clone());
                    }
                    // 其余未开始的文件标记 skipped
                    for (j, r) in t.results.iter_mut().enumerate() {
                        if j > idx && r.status == "pending" {
                            r.status = "skipped".into();
                        }
                    }
                });
                Self::finish_deploy(
                    &tasks,
                    &busy,
                    &unified,
                    &label,
                    &task_id,
                    &target.id,
                    "failed",
                    Some(format!(
                        "文件传输失败 {} → {}: {}",
                        f.local_path, f.remote_path, summary
                    )),
                );
                return;
            }
        }

        // —— 阶段三：可选远程命令 ——
        if let Some(cmd) = run_cmd.as_deref().filter(|c| !c.trim().is_empty()) {
            Self::patch_deploy(&tasks, &task_id, |t| {
                t.status = "running".into();
            });
            let started = Instant::now();
            let out = run_ssh(&target, &format!("sh -c {}", sh_quote(cmd)), cmd_timeout).await;
            let duration = started.elapsed().as_millis() as u64;
            let output = CmdOutput {
                exit_code: out.exit_code,
                stdout: out.stdout.clone(),
                stderr: out.stderr.clone(),
                duration_ms: duration,
            };
            if out.is_success() {
                Self::patch_deploy(&tasks, &task_id, |t| {
                    t.cmd_output = Some(output);
                });
            } else {
                let summary = out.err_summary();
                Self::patch_deploy(&tasks, &task_id, |t| {
                    t.cmd_output = Some(output);
                });
                Self::finish_deploy(
                    &tasks,
                    &busy,
                    &unified,
                    &label,
                    &task_id,
                    &target.id,
                    "failed",
                    Some(format!("远程命令失败（{cmd}）: {summary}")),
                );
                return;
            }
        }

        Self::finish_deploy(&tasks, &busy, &unified, &label, &task_id, &target.id, "completed", None);
    }

    /// 把 building 任务的实时 step/progress/log 合入快照（GET 端点用）。
    fn hydrate_iso_task(&self, mut task: IsoTask) -> IsoTask {
        if task.status != "building" {
            return task;
        }
        if let Some(h) = self
            .iso_builds
            .lock()
            .expect("iso_builds poisoned")
            .get(&task.id)
            .cloned()
        {
            task.step = Some(h.step.lock().expect("iso step poisoned").clone());
            task.progress = Some(*h.progress.lock().expect("iso progress poisoned"));
            task.build_log = h.log.lock().expect("iso log poisoned").clone();
        }
        task
    }

    /// 由 ISO 任务派生 os-iso 构建规格（std/clone 变体映射）。
    fn iso_spec_of(task: &IsoTask) -> Result<IsoSpec, String> {
        if task.arch != "x86_64" && task.arch != "aarch64" {
            return Err(format!("不支持的架构 {}（仅 x86_64 / aarch64）", task.arch));
        }
        let variant = if task.variant == "clone" {
            // 克隆变体内嵌配置快照：真实快照导出未接通前先空对象
            //（os-iso 侧会做敏感项过滤，见 iso.rs filter_sensitive）
            IsoVariant::Clone {
                config_snapshot: serde_json::json!({}),
            }
        } else {
            IsoVariant::Standard
        };
        Ok(IsoSpec {
            variant,
            base_image: format!("ubuntu-{}-base.squashfs", task.ubuntu_version),
            components: vec!["osd".to_string()],
            ubuntu_version: task.ubuntu_version.clone(),
            arch: task.arch.clone(),
            locale: "zh_CN.UTF-8".to_string(),
        })
    }
}

impl Default for ProvisioningRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for ProvisioningRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            // —— 一键安装引导 ——
            spec(
                HttpMethod::Get,
                "/api/v1/provisioning/install.sh",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/provisioning/prepare-distributable",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/provisioning/dist/:artifact",
                false,
                vec![],
            ),
            // —— PXE ——
            spec(
                HttpMethod::Get,
                "/api/v1/provisioning/pxe/config",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/provisioning/pxe/config",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/provisioning/pxe/boot-entries",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/provisioning/pxe/boot-entries",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/provisioning/pxe/boot-entries/:id",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/provisioning/pxe/status",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/provisioning/pxe/start",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/provisioning/pxe/stop",
                true,
                vec!["admin".into()],
            ),
            // —— ISO ——
            spec(
                HttpMethod::Get,
                "/api/v1/provisioning/iso/tasks",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/provisioning/iso/tasks",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/provisioning/iso/tasks/:id",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/provisioning/iso/tasks/:id",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/provisioning/iso/tasks/:id/build",
                true,
                vec!["admin".into()],
            ),
            // —— SSH ——
            spec(
                HttpMethod::Get,
                "/api/v1/provisioning/ssh/targets",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/provisioning/ssh/targets",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/provisioning/ssh/targets/:id",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/provisioning/ssh/targets/:id/test",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/provisioning/ssh/deploy",
                true,
                vec!["admin".into()],
            ),
            // 部署记录含远程路径与命令输出，收紧为 admin 读
            spec(
                HttpMethod::Get,
                "/api/v1/provisioning/ssh/deploys",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/provisioning/ssh/deploy/:id",
                true,
                vec!["admin".into()],
            ),
            // —— 网络装机 P0（docs/NET_BOOT.md）——
            // 注：GET /isos/:file 与 GET /boot/:id/:file（ISO/casper 流式直传）
            // 为 axum 特挂路由（http.rs build_router，Range/206/流式），不进本表
            // （同 /git/* 先例——直挂 handler 无法走 JSON 信封）。
            spec(
                HttpMethod::Get,
                "/api/v1/provisioning/bootstrap.ipxe",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/provisioning/ipxe/menu",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/provisioning/seed/:id/meta-data",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/provisioning/seed/:id/user-data",
                false,
                vec![],
            ),
            spec(HttpMethod::Get, "/api/v1/provisioning/isos", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/provisioning/isos/scan",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/provisioning/isos/:file",
                true,
                vec!["admin".into()],
            ),
            // —— 统计 ——
            spec(HttpMethod::Get, "/api/v1/provisioning/stats", false, vec![]),
        ]
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        match (req.method, segs.as_slice()) {
            // ==================== 一键安装引导 ====================
            // —— GET /api/v1/provisioning/install.sh —— 动态生成（公开，原文直传）
            (HttpMethod::Get, ["api", "v1", "provisioning", "install.sh"]) => {
                let source = source_base_url(&req);
                let bootstrap = default_bootstrap_list();
                // 双架构分发件各自已就绪 → 对应 sha256 烘焙进脚本，下载端按
                // uname -m 分流后取本架构期望值对拍；未就绪 → 空值，脚本侧
                // 跳过对拍但下载步会得到 404 指引
                let x86_path = distributable_bin_path();
                let arm_path = artifact_fs_path(DISTRIBUTABLE_AARCH64_ARTIFACT);
                let (expected_sha, expected_sha_arm) = tokio::task::spawn_blocking(move || {
                    let x86 = load_artifact(&x86_path)
                        .map(|(_, sha)| sha)
                        .unwrap_or_default();
                    let arm = load_artifact(&arm_path)
                        .map(|(_, sha)| sha)
                        .unwrap_or_default();
                    (x86, arm)
                })
                .await
                .map_err(|e| ApiGatewayError::Internal(format!("读取分发件失败: {e}")))?;
                let script =
                    render_install_script(&source, &bootstrap, &expected_sha, &expected_sha_arm);
                Ok(ApiResponse {
                    status: 200,
                    body: serde_json::Value::String(script),
                    headers: serde_json::json!({
                        "content-type": "text/x-shellscript; charset=utf-8",
                        "content-disposition": "attachment; filename=\"install-nexos.sh\"",
                    }),
                })
            }

            // —— POST /api/v1/provisioning/prepare-distributable —— 发布可分发件（admin）
            (HttpMethod::Post, ["api", "v1", "provisioning", "prepare-distributable"]) => {
                let exe = std::env::current_exe().map_err(|e| {
                    ApiGatewayError::Internal(format!("定位当前可执行文件失败: {e}"))
                })?;
                let out = distributable_bin_path();
                let mut prepared = tokio::task::spawn_blocking(move || stage_artifact(&exe, &out))
                    .await
                    .map_err(|e| ApiGatewayError::Internal(format!("prepare 任务 join 失败: {e}")))?
                    .map_err(ApiGatewayError::Internal)?;
                // 发版双通道接线（2026-09-03）：暂存成功 → 自动登记同版本更新
                // 工件。version=运行二进制的 CARGO_PKG_VERSION（暂存的正是
                // current_exe，版本号与字节同源）；登记复用 POST /update/artifact
                // 的全套校验 + sha256（ELF 魔数/体积门槛/绝对路径），重复 version
                // 覆盖 → 重跑 prepare 幂等。失败不拦 prepare（dist 主通道不受
                // 影响），原因回传响应字段。
                if let Some(registry) = self.update_registry.clone() {
                    let version = env!("CARGO_PKG_VERSION").to_string();
                    let path = prepared.path.clone();
                    let joined = tokio::task::spawn_blocking(move || {
                        registry.register_artifact_and_persist(&version, &path)
                    })
                    .await;
                    match joined {
                        Ok(Ok(a)) => prepared.update_artifact = Some(a),
                        Ok(Err(e)) => {
                            eprintln!(
                                "[provisioning] prepare 自动登记更新工件失败（分发通道不受影响）: {e}"
                            );
                            prepared.update_artifact_error = Some(e);
                        }
                        Err(e) => {
                            let msg = format!("登记任务 join 失败: {e}");
                            eprintln!("[provisioning] {msg}");
                            prepared.update_artifact_error = Some(msg);
                        }
                    }
                }
                Ok(ok_json(to_value(&prepared)?))
            }

            // —— GET /api/v1/provisioning/dist/:artifact —— 分发下载（公开）
            (HttpMethod::Get, ["api", "v1", "provisioning", "dist", artifact]) => {
                // 防穿越：精确白名单，任何路径注入形态（../、%2e%2e、绝对路径…）都
                // 无法命中白名单名——不存在基于用户输入的路径拼接
                if !DISTRIBUTABLE_ARTIFACTS.contains(artifact) {
                    return Ok(error_response(
                        400,
                        &format!(
                            "未知工件: {artifact}（可用: {}）",
                            DISTRIBUTABLE_ARTIFACTS.join(", ")
                        ),
                    ));
                }
                let (bin_path, file_name) = (artifact_fs_path(artifact), artifact.to_string());
                let blob = tokio::task::spawn_blocking(move || load_artifact(&bin_path))
                    .await
                    .map_err(|e| ApiGatewayError::Internal(format!("读取工件失败: {e}")))?;
                match blob {
                    Some((bytes, sha256)) => Ok(ApiResponse {
                        status: 200,
                        // base64 装载 → 网关直传通道按 content-type 解码回原始字节
                        body: serde_json::Value::String(
                            base64::engine::general_purpose::STANDARD.encode(&bytes),
                        ),
                        headers: serde_json::json!({
                            "content-type": "application/octet-stream",
                            "x-nexos-sha256": sha256,
                            "content-disposition":
                                format!("attachment; filename=\"{file_name}\""),
                        }),
                    }),
                    None => {
                        let hint = if *artifact == DISTRIBUTABLE_AARCH64_ARTIFACT {
                            format!(
                                "aarch64 工件未就绪：请在源节点跑 scripts/release.sh 刷新 {}",
                                DISTRIBUTABLE_AARCH64_REL_PATH
                            )
                        } else {
                            "工件未就绪：请先在源节点执行 POST /api/v1/provisioning/prepare-distributable"
                                .to_string()
                        };
                        Ok(error_response(404, &hint))
                    }
                }
            }

            // ==================== PXE ====================
            // —— GET /api/v1/provisioning/pxe/config ——
            (HttpMethod::Get, ["api", "v1", "provisioning", "pxe", "config"]) => {
                let cfg = self.pxe_config.lock().expect("pxe_config poisoned").clone();
                Ok(ok_json(to_value(&cfg)?))
            }

            // —— POST /api/v1/provisioning/pxe/config ——
            (HttpMethod::Post, ["api", "v1", "provisioning", "pxe", "config"]) => {
                let cfg: PxeConfig = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析 PXE 配置请求体失败: {e}"))
                })?;
                *self.pxe_config.lock().expect("pxe_config poisoned") = cfg.clone();
                Ok(ApiResponse {
                    status: 200,
                    body: to_value(&cfg)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/provisioning/pxe/boot-entries ——
            (HttpMethod::Get, ["api", "v1", "provisioning", "pxe", "boot-entries"]) => {
                let list = self
                    .boot_entries
                    .lock()
                    .expect("boot_entries poisoned")
                    .clone();
                Ok(ok_json(to_value(&list)?))
            }

            // —— POST /api/v1/provisioning/pxe/boot-entries ——
            (HttpMethod::Post, ["api", "v1", "provisioning", "pxe", "boot-entries"]) => {
                let entry: BootEntry = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析启动条目请求体失败: {e}"))
                })?;
                // 清 default + 入表在同一锁作用域内完成（避免并发插入交错）
                {
                    let mut list = self.boot_entries.lock().expect("boot_entries poisoned");
                    if entry.default_entry {
                        for e in list.iter_mut() {
                            e.default_entry = false;
                        }
                    }
                    list.push(entry.clone());
                }
                Ok(ApiResponse {
                    status: 201,
                    body: to_value(&entry)?,
                    headers: serde_json::json!({}),
                })
            }

            // —— DELETE /api/v1/provisioning/pxe/boot-entries/:id ——
            (HttpMethod::Delete, ["api", "v1", "provisioning", "pxe", "boot-entries", id]) => {
                let mut list = self.boot_entries.lock().expect("boot_entries poisoned");
                let before = list.len();
                list.retain(|e| e.id != *id);
                if list.len() == before {
                    return Ok(error_response(404, &format!("启动条目不存在: {id}")));
                }
                Ok(ApiResponse {
                    status: 204,
                    body: serde_json::Value::Null,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/provisioning/pxe/status ——
            (HttpMethod::Get, ["api", "v1", "provisioning", "pxe", "status"]) => {
                let running = *self.pxe_running.lock().expect("pxe_running poisoned");
                let status = PxeStatus {
                    running,
                    state: if running {
                        "running".into()
                    } else {
                        "stopped".into()
                    },
                };
                Ok(ok_json(to_value(&status)?))
            }

            // —— POST /api/v1/provisioning/pxe/start ——
            (HttpMethod::Post, ["api", "v1", "provisioning", "pxe", "start"]) => {
                *self.pxe_running.lock().expect("pxe_running poisoned") = true;
                let status = PxeStatus {
                    running: true,
                    state: "running".into(),
                };
                Ok(ok_json(to_value(&status)?))
            }

            // —— POST /api/v1/provisioning/pxe/stop ——
            (HttpMethod::Post, ["api", "v1", "provisioning", "pxe", "stop"]) => {
                *self.pxe_running.lock().expect("pxe_running poisoned") = false;
                let status = PxeStatus {
                    running: false,
                    state: "stopped".into(),
                };
                Ok(ok_json(to_value(&status)?))
            }

            // ==================== ISO ====================
            // —— GET /api/v1/provisioning/iso/tasks —— 列任务
            (HttpMethod::Get, ["api", "v1", "provisioning", "iso", "tasks"]) => {
                let list: Vec<IsoTask> = self
                    .iso_tasks
                    .lock()
                    .expect("iso_tasks poisoned")
                    .iter()
                    .map(|t| self.hydrate_iso_task(t.clone()))
                    .collect();
                Ok(ok_json(to_value(&list)?))
            }

            // —— POST /api/v1/provisioning/iso/tasks —— 建任务（不触发构建）
            (HttpMethod::Post, ["api", "v1", "provisioning", "iso", "tasks"]) => {
                let body: CreateIsoTaskBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析 ISO 任务请求体失败: {e}"))
                })?;
                if body.name.trim().is_empty() {
                    return Ok(error_response(400, "name 不可为空"));
                }
                let task = IsoTask {
                    id: self.next_id("iso"),
                    name: body.name,
                    version: body
                        .version
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "0.1.0".to_string()),
                    variant: body
                        .variant
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "std".to_string()),
                    arch: body
                        .arch
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "x86_64".to_string()),
                    ubuntu_version: body
                        .ubuntu_version
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "26.04".to_string()),
                    status: "pending".into(),
                    iso_path: None,
                    sha256: None,
                    size_bytes: None,
                    created_at: now_iso(),
                    error: None,
                    step: None,
                    progress: None,
                    build_log: Vec::new(),
                };
                let resp_body = to_value(&task)?;
                // 统一任务框架旁路登记（queued；内存 Vec 推进保留不动）
                self.unified_register_iso(&task);
                self.iso_tasks
                    .lock()
                    .expect("iso_tasks poisoned")
                    .push(task);
                Ok(ApiResponse {
                    status: 201,
                    body: resp_body,
                    headers: serde_json::json!({}),
                })
            }

            // —— DELETE /api/v1/provisioning/iso/tasks/:id —— 删任务
            (HttpMethod::Delete, ["api", "v1", "provisioning", "iso", "tasks", id]) => {
                let mut tasks = self.iso_tasks.lock().expect("iso_tasks poisoned");
                let before = tasks.len();
                tasks.retain(|t| t.id != *id);
                if tasks.len() == before {
                    return Ok(error_response(404, &format!("ISO 任务不存在: {id}")));
                }
                drop(tasks);
                self.iso_builds
                    .lock()
                    .expect("iso_builds poisoned")
                    .remove(*id);
                Ok(ApiResponse {
                    status: 204,
                    body: serde_json::Value::Null,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/provisioning/iso/tasks/:id —— 单任务详情（含实时构建进度）
            (HttpMethod::Get, ["api", "v1", "provisioning", "iso", "tasks", id]) => {
                let tasks = self.iso_tasks.lock().expect("iso_tasks poisoned");
                match tasks.iter().find(|t| t.id == *id) {
                    Some(t) => {
                        let hydrated = self.hydrate_iso_task(t.clone());
                        Ok(ok_json(to_value(&hydrated)?))
                    }
                    None => Ok(error_response(404, &format!("ISO 任务不存在: {id}"))),
                }
            }

            // —— POST /api/v1/provisioning/iso/tasks/:id/build —— 真实驱动构建
            (HttpMethod::Post, ["api", "v1", "provisioning", "iso", "tasks", id, "build"]) => {
                // 状态预检：只允许 pending / failed 触发
                let task_snapshot = {
                    let tasks = self.iso_tasks.lock().expect("iso_tasks poisoned");
                    match tasks.iter().find(|t| t.id == *id) {
                        Some(t) => t.clone(),
                        None => {
                            return Ok(error_response(404, &format!("ISO 任务不存在: {id}")));
                        }
                    }
                };
                if task_snapshot.status == "building" {
                    return Ok(error_response(409, "该任务正在构建中"));
                }
                if task_snapshot.status == "completed" {
                    return Ok(error_response(409, "该任务已构建完成；如需重建请新建任务"));
                }

                let handle = IsoBuildHandle::new();
                let root = self.iso_output_root.clone();

                // 工具链探测（xorriso/mksquashfs）→ 缺失即 failed 附安装指引
                let env = os_iso::env::IsoEnvironment::probe();
                if !env.is_capable() {
                    let missing = env.missing_tools().join(", ");
                    let err = format!(
                        "构建工具链缺失: {missing}。安装: sudo apt install xorriso squashfs-tools \
                         （产物目录 {}）",
                        root.display()
                    );
                    handle.push_log(err.clone());
                    let log_snapshot = handle.log.lock().expect("iso log poisoned").clone();
                    let mut tasks = self.iso_tasks.lock().expect("iso_tasks poisoned");
                    if let Some(t) = tasks.iter_mut().find(|t| t.id == *id) {
                        t.status = "failed".into();
                        t.error = Some(err.clone());
                        t.build_log = log_snapshot;
                        let body = to_value(t)?;
                        drop(tasks);
                        Self::unified_finish(
                            &self.unified,
                            "iso",
                            id,
                            "failed",
                            &task_snapshot.name,
                            None,
                            Some(&err),
                        );
                        return Ok(ok_json(body));
                    }
                    return Ok(error_response(404, &format!("ISO 任务不存在: {id}")));
                }

                // 产物根目录可写性预检
                if let Err(e) = std::fs::create_dir_all(&root) {
                    let err = format!("产物目录不可创建（{}）: {e}", root.display());
                    let mut tasks = self.iso_tasks.lock().expect("iso_tasks poisoned");
                    if let Some(t) = tasks.iter_mut().find(|t| t.id == *id) {
                        t.status = "failed".into();
                        t.error = Some(err.clone());
                        let body = to_value(t)?;
                        drop(tasks);
                        Self::unified_finish(
                            &self.unified,
                            "iso",
                            id,
                            "failed",
                            &task_snapshot.name,
                            None,
                            Some(&err),
                        );
                        return Ok(ok_json(body));
                    }
                    return Ok(error_response(404, &format!("ISO 任务不存在: {id}")));
                }

                // 构建规格派生（架构非法等 → failed）
                let spec = match Self::iso_spec_of(&task_snapshot) {
                    Ok(s) => s,
                    Err(e) => {
                        let mut tasks = self.iso_tasks.lock().expect("iso_tasks poisoned");
                        if let Some(t) = tasks.iter_mut().find(|t| t.id == *id) {
                            t.status = "failed".into();
                            t.error = Some(e.clone());
                            let body = to_value(t)?;
                            drop(tasks);
                            Self::unified_finish(
                                &self.unified,
                                "iso",
                                id,
                                "failed",
                                &task_snapshot.name,
                                None,
                                Some(&e),
                            );
                            return Ok(ok_json(body));
                        }
                        return Ok(error_response(404, &format!("ISO 任务不存在: {id}")));
                    }
                };

                // 标记 building + 注册观测句柄
                handle.push_log(format!(
                    "开始构建 {}（{} {}）产物目录 {}",
                    task_snapshot.name,
                    task_snapshot.variant,
                    task_snapshot.ubuntu_version,
                    root.display()
                ));
                handle.push_log(
                    "提示：真实构建依赖已准备的 rootfs 源树与引导文件；源树缺失时子进程将以失败告终，stderr 见本日志"
                        .to_string(),
                );
                {
                    let mut tasks = self.iso_tasks.lock().expect("iso_tasks poisoned");
                    if let Some(t) = tasks.iter_mut().find(|t| t.id == *id) {
                        t.status = "building".into();
                        t.error = None;
                        t.iso_path = None;
                        t.sha256 = None;
                        t.size_bytes = None;
                    }
                }
                // 统一登记：building → running（stage=building）
                if let Some(store) = &self.unified {
                    store.progress("iso", id, Some("building"), "");
                }
                self.iso_builds
                    .lock()
                    .expect("iso_builds poisoned")
                    .insert((*id).to_string(), handle.clone());

                // 后台执行（os-iso XorrisoIsoBuilder + LoggingIsoRunner）
                let tasks_arc = self.iso_tasks.clone();
                let builds_arc = self.iso_builds.clone();
                let iso_id = (*id).to_string();
                let unified_arc = self.unified.clone();
                let iso_label = format!(
                    "ISO 构建 {}（{} {} {}）",
                    task_snapshot.name, task_snapshot.variant, task_snapshot.arch,
                    task_snapshot.ubuntu_version
                );
                tokio::spawn(async move {
                    let runner = LoggingIsoRunner::new(handle.clone());
                    let builder = XorrisoIsoBuilder::new(root.clone(), Arc::new(runner));
                    let outcome = builder.build(spec).await;
                    let log_snapshot = handle.log.lock().expect("iso log poisoned").clone();
                    match outcome {
                        Ok(tid) => {
                            let status = builder.status(&tid).await;
                            let mut tasks = tasks_arc.lock().expect("iso_tasks poisoned");
                            if let Some(t) = tasks.iter_mut().find(|t| t.id == iso_id) {
                                t.build_log = log_snapshot;
                                match status {
                                    IsoBuildStatus::Completed(r) => {
                                        t.status = "completed".into();
                                        t.iso_path =
                                            Some(r.iso_path.to_string_lossy().into_owned());
                                        t.sha256 = Some(r.sha256.clone());
                                        t.size_bytes = Some(r.size_bytes);
                                        t.step = None;
                                        t.progress = Some(1.0);
                                        Self::unified_finish(
                                            &unified_arc,
                                            "iso",
                                            &iso_id,
                                            "completed",
                                            &iso_label,
                                            Some(&r.iso_path.to_string_lossy()),
                                            None,
                                        );
                                    }
                                    IsoBuildStatus::Failed { reason } => {
                                        t.status = "failed".into();
                                        t.error = Some(reason.clone());
                                        Self::unified_finish(
                                            &unified_arc,
                                            "iso",
                                            &iso_id,
                                            "failed",
                                            &iso_label,
                                            None,
                                            Some(&reason),
                                        );
                                    }
                                    _ => {
                                        t.status = "failed".into();
                                        t.error = Some("构建异常结束（无产物）".to_string());
                                        Self::unified_finish(
                                            &unified_arc,
                                            "iso",
                                            &iso_id,
                                            "failed",
                                            &iso_label,
                                            None,
                                            Some("构建异常结束（无产物）"),
                                        );
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let mut tasks = tasks_arc.lock().expect("iso_tasks poisoned");
                            if let Some(t) = tasks.iter_mut().find(|t| t.id == iso_id) {
                                t.status = "failed".into();
                                t.error = Some(e.to_string());
                                t.build_log = log_snapshot;
                                Self::unified_finish(
                                    &unified_arc,
                                    "iso",
                                    &iso_id,
                                    "failed",
                                    &iso_label,
                                    None,
                                    Some(&e.to_string()),
                                );
                            }
                        }
                    }
                    builds_arc
                        .lock()
                        .expect("iso_builds poisoned")
                        .remove(&iso_id);
                });

                // 返回 building 态任务（前端轮询详情取进度/日志）
                let hydrated = {
                    let tasks = self.iso_tasks.lock().expect("iso_tasks poisoned");
                    match tasks.iter().find(|t| t.id == *id) {
                        Some(t) => self.hydrate_iso_task(t.clone()),
                        None => {
                            return Ok(error_response(404, &format!("ISO 任务不存在: {id}")));
                        }
                    }
                };
                Ok(ok_json(to_value(&hydrated)?))
            }

            // ==================== SSH ====================
            // —— GET /api/v1/provisioning/ssh/targets —— 列目标
            (HttpMethod::Get, ["api", "v1", "provisioning", "ssh", "targets"]) => {
                let list = self
                    .ssh_targets
                    .lock()
                    .expect("ssh_targets poisoned")
                    .clone();
                Ok(ok_json(to_value(&list)?))
            }

            // —— POST /api/v1/provisioning/ssh/targets —— 添加目标
            (HttpMethod::Post, ["api", "v1", "provisioning", "ssh", "targets"]) => {
                let body: CreateSshTargetBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析 SSH 目标请求体失败: {e}"))
                })?;
                if body.name.trim().is_empty() {
                    return Ok(error_response(400, "name 不可为空"));
                }
                if body.host.trim().is_empty() {
                    return Ok(error_response(400, "host 不可为空"));
                }
                let port = body.port.unwrap_or(22);
                let user = body
                    .user
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "root".to_string());
                let private_key_path = body
                    .private_key_path
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                let target = SshTarget {
                    id: self.next_id("ssh"),
                    name: body.name,
                    host: body.host,
                    port,
                    user,
                    private_key_path,
                    status: "unknown".into(),
                    last_checked: None,
                    created_at: now_iso(),
                };
                let resp_body = to_value(&target)?;
                self.ssh_targets
                    .lock()
                    .expect("ssh_targets poisoned")
                    .push(target);
                Ok(ApiResponse {
                    status: 201,
                    body: resp_body,
                    headers: serde_json::json!({}),
                })
            }

            // —— DELETE /api/v1/provisioning/ssh/targets/:id —— 删目标
            (HttpMethod::Delete, ["api", "v1", "provisioning", "ssh", "targets", id]) => {
                let mut targets = self.ssh_targets.lock().expect("ssh_targets poisoned");
                let before = targets.len();
                targets.retain(|t| t.id != *id);
                if targets.len() == before {
                    return Ok(error_response(404, &format!("SSH 目标不存在: {id}")));
                }
                Ok(ApiResponse {
                    status: 204,
                    body: serde_json::Value::Null,
                    headers: serde_json::json!({}),
                })
            }

            // —— POST /api/v1/provisioning/ssh/targets/:id/test —— 测试连接（真实调 ssh）
            (HttpMethod::Post, ["api", "v1", "provisioning", "ssh", "targets", id, "test"]) => {
                // 先快照 target，立即释放锁
                let target_snapshot = {
                    let targets = self.ssh_targets.lock().expect("ssh_targets poisoned");
                    targets.iter().find(|t| t.id == *id).cloned()
                };
                let target = match target_snapshot {
                    Some(t) => t,
                    None => {
                        return Ok(error_response(404, &format!("SSH 目标不存在: {id}")));
                    }
                };
                // 真实测试连接（BatchMode=yes 禁密码）
                let reachable = Self::test_ssh_connection(&target).await;
                let now = now_iso();
                let mut targets = self.ssh_targets.lock().expect("ssh_targets poisoned");
                if let Some(t) = targets.iter_mut().find(|t| t.id == target.id) {
                    t.status = if reachable {
                        "reachable"
                    } else {
                        "unreachable"
                    }
                    .into();
                    t.last_checked = Some(now);
                }
                let updated = targets
                    .iter()
                    .find(|t| t.id == target.id)
                    .cloned()
                    .unwrap_or(target);
                Ok(ok_json(to_value(&updated)?))
            }

            // —— POST /api/v1/provisioning/ssh/deploy —— 发起部署（真实 scp/ssh 执行）
            (HttpMethod::Post, ["api", "v1", "provisioning", "ssh", "deploy"]) => {
                let body: CreateDeployBody = serde_json::from_value(req.body)
                    .map_err(|e| ApiGatewayError::Internal(format!("解析部署请求体失败: {e}")))?;
                let files: Vec<FileTransfer> = body
                    .files
                    .into_iter()
                    .filter(|f| !f.local_path.trim().is_empty() && !f.remote_path.trim().is_empty())
                    .collect();
                let run_cmd = body
                    .run_cmd
                    .map(|c| c.trim().to_string())
                    .filter(|c| !c.is_empty());
                if files.is_empty() && run_cmd.is_none() {
                    return Ok(error_response(400, "files 与 run_cmd 至少提供一项"));
                }

                // 校验 target 存在并快照
                let target = {
                    let targets = self.ssh_targets.lock().expect("ssh_targets poisoned");
                    match targets.iter().find(|t| t.id == body.target_id) {
                        Some(t) => t.clone(),
                        None => {
                            return Ok(error_response(
                                404,
                                &format!("SSH 目标不存在: {}", body.target_id),
                            ));
                        }
                    }
                };

                // 同目标互斥：进行中的部署存在 → 409（附任务 id 便于前端跳转跟进）。
                // 注意锁序：busy 单独短临界区，不与 deploy_tasks/counter 嵌套
                // （finish_deploy 是 deploy_tasks→busy 的顺序获取，反向嵌套会死锁）。
                let task_id = {
                    let mut busy = self.busy_deploys.lock().expect("busy_deploys poisoned");
                    if let Some(running_id) = busy.get(&target.id).cloned() {
                        return Ok(ApiResponse {
                            status: 409,
                            body: serde_json::json!({
                                "error": format!("目标 {} 已有部署任务进行中", target.name),
                                "deploy_id": running_id,
                            }),
                            headers: serde_json::json!({}),
                        });
                    }
                    let id = self.next_id("deploy");
                    busy.insert(target.id.clone(), id.clone());
                    id
                };

                let task = DeployTask {
                    id: task_id.clone(),
                    target_id: target.id.clone(),
                    results: files
                        .iter()
                        .map(|f| FileTransferResult {
                            local_path: f.local_path.clone(),
                            remote_path: f.remote_path.clone(),
                            status: "pending".into(),
                            exit_code: None,
                            duration_ms: None,
                            error: None,
                        })
                        .collect(),
                    files,
                    run_cmd,
                    status: "pending".into(),
                    created_at: now_iso(),
                    error: None,
                    cmd_output: None,
                    started_at: None,
                    finished_at: None,
                };
                let resp_body = to_value(&task)?;
                // 统一登记（queued；deploy_tasks 内存 Vec 保留不动）
                if let Some(store) = &self.unified {
                    store.register(&UnifiedTask {
                        id: task_id.clone(),
                        kind: "deploy".into(),
                        status: UnifiedStatus::Queued,
                        label: format!(
                            "SSH 部署 → {}（{}@{}）",
                            target.name, target.user, target.host
                        ),
                        stage: None,
                        log_tail: String::new(),
                        output: None,
                        created_at: now_iso(),
                        finished_at: None,
                        error: None,
                    });
                }
                {
                    let mut tasks = self.deploy_tasks.lock().expect("deploy_tasks poisoned");
                    tasks.push(task);
                    while tasks.len() > DEPLOY_TASKS_MAX {
                        tasks.remove(0);
                    }
                }
                tokio::spawn(Self::run_deploy(
                    self.deploy_tasks.clone(),
                    self.busy_deploys.clone(),
                    target,
                    task_id,
                    self.deploy_file_timeout,
                    self.deploy_cmd_timeout,
                    self.unified.clone(),
                ));
                Ok(ApiResponse {
                    status: 201,
                    body: resp_body,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/provisioning/ssh/deploys —— 部署任务列表（admin）
            (HttpMethod::Get, ["api", "v1", "provisioning", "ssh", "deploys"]) => {
                let tasks = self.deploy_tasks.lock().expect("deploy_tasks poisoned");
                // 最新在前
                let mut list: Vec<DeployTask> = tasks.clone();
                list.reverse();
                Ok(ok_json(to_value(&list)?))
            }

            // —— GET /api/v1/provisioning/ssh/deploy/:id —— 部署任务状态（admin）
            (HttpMethod::Get, ["api", "v1", "provisioning", "ssh", "deploy", id]) => {
                let tasks = self.deploy_tasks.lock().expect("deploy_tasks poisoned");
                match tasks.iter().find(|t| t.id == *id) {
                    Some(t) => Ok(ok_json(to_value(t)?)),
                    None => Ok(error_response(404, &format!("部署任务不存在: {id}"))),
                }
            }

            // ==================== 网络装机 P0 ====================
            // —— GET /api/v1/provisioning/bootstrap.ipxe —— U 盘 iPXE 第一跳脚本
            // （公开；text/plain 原文直传—— iPXE chain 拉取）
            (HttpMethod::Get, ["api", "v1", "provisioning", "bootstrap.ipxe"]) => {
                let script = render_bootstrap_ipxe();
                Ok(ApiResponse {
                    status: 200,
                    body: serde_json::Value::String(script),
                    headers: serde_json::json!({
                        "content-type": "text/plain; charset=utf-8",
                        "content-disposition": "attachment; filename=\"bootstrap.ipxe\"",
                    }),
                })
            }

            // —— GET /api/v1/provisioning/ipxe/menu?arch=&platform= —— 动态菜单
            // （公开；按仓内最新稳定版实时渲染，滚动版本策略只列 latest）
            (HttpMethod::Get, ["api", "v1", "provisioning", "ipxe", "menu"]) => {
                let (arch_q, platform_q) = query_params(&req.path);
                let arch = map_ipxe_buildarch(arch_q.as_deref().unwrap_or(""));
                let platform = platform_q.unwrap_or_else(|| "pcbios".to_string());
                let base = netboot_base_url(&req);
                let repo = self.netboot_repo_snapshot();
                let items: Vec<(String, String, String)> = repo
                    .iter()
                    .filter(|f| f.latest && f.arch == arch)
                    .map(|f| (f.version.clone(), f.arch.clone(), f.id.clone()))
                    .collect();
                let script = render_ipxe_menu(&items, arch, &platform, &base);
                Ok(ApiResponse {
                    status: 200,
                    body: serde_json::Value::String(script),
                    headers: serde_json::json!({
                        "content-type": "text/plain; charset=utf-8",
                    }),
                })
            }

            // —— GET /api/v1/provisioning/seed/:id/meta-data —— nocloud 空 meta
            (HttpMethod::Get, ["api", "v1", "provisioning", "seed", id, "meta-data"]) => {
                if parse_repo_seed_id(id).is_none() {
                    return Ok(error_response(404, &format!("未知种子条目: {id}")));
                }
                Ok(ApiResponse {
                    status: 200,
                    body: serde_json::Value::String(String::new()),
                    headers: serde_json::json!({
                        "content-type": "text/plain; charset=utf-8",
                    }),
                })
            }

            // —— GET /api/v1/provisioning/seed/:id/user-data —— autoinstall 种子
            (HttpMethod::Get, ["api", "v1", "provisioning", "seed", id, "user-data"]) => {
                let Some((version, arch, _)) = parse_repo_seed_id(id) else {
                    return Ok(error_response(404, &format!("未知种子条目: {id}")));
                };
                let base = netboot_base_url(&req);
                let params = AutoinstallSeedParams {
                    version,
                    arch,
                    server_base: base,
                    ..AutoinstallSeedParams::default()
                };
                match render_seed_user_data(&params) {
                    Ok(yaml) => Ok(ApiResponse {
                        status: 200,
                        body: serde_json::Value::String(yaml),
                        headers: serde_json::json!({
                            "content-type": "text/plain; charset=utf-8",
                        }),
                    }),
                    Err(e) => Ok(error_response(400, &e)),
                }
            }

            // —— GET /api/v1/provisioning/isos —— 仓清单（版本/架构/size/
            // sha256/提取状态/latest 标记；未 scan 过现场扫目录）
            (HttpMethod::Get, ["api", "v1", "provisioning", "isos"]) => {
                let repo = self.netboot_repo_snapshot();
                Ok(ok_json(serde_json::json!({
                    "repo_root": netboot_repo_root().display().to_string(),
                    "boot_media": NETBOOT_BOOT_MEDIA_FILES,
                    "files": repo,
                })))
            }

            // —— POST /api/v1/provisioning/isos/scan —— 登记/校验 + 惰性提取
            // 触发（admin；立即返回登记态，sha256 实算与 casper 提取后台执行，
            // GET /isos 轮询观测）
            (HttpMethod::Post, ["api", "v1", "provisioning", "isos", "scan"]) => {
                let merged = self.netboot_scan_and_process();
                Ok(ApiResponse {
                    status: 200,
                    body: serde_json::json!({
                        "scanned": merged.len(),
                        "files": merged,
                        "note": "sha256 对账与 casper 提取已入后台队列（GET /api/v1/provisioning/isos 轮询）",
                    }),
                    headers: serde_json::json!({}),
                })
            }

            // —— DELETE /api/v1/provisioning/isos/:file —— 滚动版本策略的
            // "删旧版"操作（admin；删 ISO + sidecar + boot/<id>/ 提取件）
            (HttpMethod::Delete, ["api", "v1", "provisioning", "isos", file]) => {
                let Some((_, _, id)) = parse_repo_iso_filename(file) else {
                    return Ok(error_response(
                        400,
                        &format!("未知仓文件（白名单外名字）: {file}"),
                    ));
                };
                let iso = netboot_iso_path(file);
                if !iso.is_file() {
                    return Ok(error_response(404, &format!("仓内无此文件: {file}")));
                }
                let mut removed: Vec<String> = Vec::new();
                for p in [
                    iso.clone(),
                    netboot_isos_dir().join(format!("{file}{ISO_SHA256_SIDECAR_SUFFIX}")),
                ] {
                    if std::fs::remove_file(&p).is_ok() {
                        removed.push(p.display().to_string());
                    }
                }
                let bdir = netboot_boot_dir(&id);
                if bdir.is_dir() && std::fs::remove_dir_all(&bdir).is_ok() {
                    removed.push(bdir.display().to_string());
                }
                // 登记态同步剔除
                self.netboot_repo
                    .lock()
                    .expect("netboot_repo poisoned")
                    .retain(|f| f.file != *file);
                Ok(ApiResponse {
                    status: 200,
                    body: serde_json::json!({
                        "removed": removed,
                        "note": "滚动版本策略：新版入仓（重下+scan）→ 删旧版（本端点）",
                    }),
                    headers: serde_json::json!({}),
                })
            }

            // ==================== 统计 ====================
            // —— GET /api/v1/provisioning/stats —— 聚合统计
            (HttpMethod::Get, ["api", "v1", "provisioning", "stats"]) => {
                Ok(ok_json(to_value(&self.stats_snapshot())?))
            }

            // —— 未覆盖路由 —— 兜底 404
            _ => Ok(error_response(404, "provisioning: 未匹配的路由")),
        }
    }
}

// ----------------------------------------------------------------------------
// 内部辅助
// ----------------------------------------------------------------------------

/// 一键安装脚本模板。占位符（由 [`render_install_script`] 替换，均为单引号
/// 字面量赋值，值经 `strip_single_quotes` 净化）：
///
/// - `@@SOURCE_URL@@`：安装源 base URL（Host 头/通告地址推导）
/// - `@@BOOTSTRAP@@`：P2P bootstrap 缺省列表
/// - `@@EXPECTED_SHA256@@`：x86_64 分发件 sha256（未 prepare 时为空 → 跳过对拍）
/// - `@@EXPECTED_SHA256_AARCH64@@`：aarch64 分发件 sha256（同上；脚本按
///   `uname -m` 分流后取本架构期望值对拍）
pub const INSTALL_SCRIPT_TEMPLATE: &str = r#"#!/usr/bin/env bash
# ============================================================
# NexOS 一键安装引导
# （由源节点 GET /api/v1/provisioning/install.sh 动态生成；仓库同源副本：
#   scripts/install-nexos.sh）
#
# 架构自动分流：脚本开头 uname -m 探测——x86_64 下载 dist/os-api，
# aarch64（DGX Spark 等 ARM 机）下载 dist/os-api-aarch64，其他架构报错终止。
#
# 用法（在一台全新的 NAT 后 Ubuntu 22.04/24.04 上执行一条命令完成安装入网）：
#   sudo bash -c "$(curl -fsSL http://<任一公网入口>:8558/api/v1/provisioning/install.sh)"
#
# 参数：
#   --source URL      安装源 HTTP 入口（缺省即本脚本的来源）
#   --name NAME       节点昵称（缺省 = 主机名）
#   --token TOKEN     NEXOS_ADMIN_TOKEN（缺省 change-me-admin-token，装完请更换）
#   --bootstrap LIST  P2P 引导节点，逗号分隔 host:port
#   --port PORT       os-api HTTP 监听端口（缺省 8558）
#   --force           二进制已存在也无条件重新下载（版本一致也强制刷新）
#
# 幂等：重复执行安全（已存在的步骤自动跳过）。**版本感知升级**：本地二进制
# sha256 与源端分发件（烘焙进本脚本的期望值）一致 → 跳过下载；不一致 → 自动
# 重下替换并提示"升级 X→Y"。老版本脚本（0.1.4 前生成）无此逻辑，首次升级需 --force。
# 卸载：systemctl disable --now nexos-os-api && rm -rf /opt/nexos \
#       /etc/systemd/system/nexos-os-api.service
# ============================================================
set -euo pipefail

NEXOS_SOURCE_DEFAULT='@@SOURCE_URL@@'
NEXOS_BOOTSTRAP_DEFAULT='@@BOOTSTRAP@@'
NEXOS_SHA256_EXPECTED='@@EXPECTED_SHA256@@'
NEXOS_SHA256_EXPECTED_AARCH64='@@EXPECTED_SHA256_AARCH64@@'

SRC='' BOOTSTRAP='' NAME='' TOKEN='' PORT='8558' FORCE=0

log()  { printf '\033[32m[nexos]\033[0m %s\n' "$*"; }
warn() { printf '\033[33m[nexos]\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[31m[nexos]\033[0m %s\n' "$*" >&2; exit 1; }

# —— 0) 架构分流（uname -m 自动探测；DGX Spark 等 aarch64 主机同一条命令）——
MACHINE_ARCH="$(uname -m)"
case "$MACHINE_ARCH" in
  x86_64)  ARTIFACT='os-api';         EXPECTED_SHA="$NEXOS_SHA256_EXPECTED" ;;
  aarch64) ARTIFACT='os-api-aarch64'; EXPECTED_SHA="$NEXOS_SHA256_EXPECTED_AARCH64" ;;
  *)
    die "不支持的架构: $MACHINE_ARCH（当前可用: x86_64 → os-api, aarch64 → os-api-aarch64）"
    ;;
esac

usage() {
  cat <<'USAGE'
用法: install-nexos.sh [--source URL] [--name NAME] [--token TOKEN]
                       [--bootstrap LIST] [--port PORT] [--force]
USAGE
  exit 0
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --source)    [[ $# -ge 2 ]] || die '--source 需要参数'; SRC="$2"; shift 2 ;;
    --name)      [[ $# -ge 2 ]] || die '--name 需要参数'; NAME="$2"; shift 2 ;;
    --token)     [[ $# -ge 2 ]] || die '--token 需要参数'; TOKEN="$2"; shift 2 ;;
    --bootstrap) [[ $# -ge 2 ]] || die '--bootstrap 需要参数'; BOOTSTRAP="$2"; shift 2 ;;
    --port)      [[ $# -ge 2 ]] || die '--port 需要参数'; PORT="$2"; shift 2 ;;
    --force)     FORCE=1; shift ;;
    -h|--help)   usage ;;
    *) die "未知参数: $1（--help 查看用法）" ;;
  esac
done

[[ $EUID -eq 0 ]] || die '需要 root：sudo bash -c "$(curl -fsSL <安装源>/api/v1/provisioning/install.sh)"'

SRC="${SRC:-$NEXOS_SOURCE_DEFAULT}"
case "$SRC" in http://*|https://*) : ;; *) SRC="http://$SRC" ;; esac
SRC="${SRC%/}"

INSTALL_DIR=/opt/nexos
BIN_PATH="$INSTALL_DIR/os-api"
UNIT_PATH=/etc/systemd/system/nexos-os-api.service
SERVICE_NAME=nexos-os-api

# —— 1) 系统依赖（幂等：缺什么装什么）——
NEED=()
command -v curl    >/dev/null 2>&1 || NEED+=(curl)
command -v git     >/dev/null 2>&1 || NEED+=(git)
command -v crontab >/dev/null 2>&1 || NEED+=(cron)
command -v ssh     >/dev/null 2>&1 || NEED+=(openssh-client)
dpkg -s ca-certificates >/dev/null 2>&1 || NEED+=(ca-certificates)
if [[ ${#NEED[@]} -gt 0 ]]; then
  log "安装系统依赖: ${NEED[*]}"
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq || warn 'apt-get update 失败（继续尝试安装）'
  apt-get install -y -qq "${NEED[@]}" || die 'apt 安装依赖失败'
else
  log '系统依赖齐备，跳过 apt 安装'
fi

# —— 2) 本机公网出口 IP（写进 NEXOS_GIT_ADVERTISE_HOST，供集群内他方寻址）——
EGRESS_IP="$(ip -4 route get 1.1.1.1 2>/dev/null | grep -oE 'src [0-9.]+' | awk '{print $2}' | head -n1 || true)"
if [[ -z $EGRESS_IP ]]; then
  EGRESS_IP="$(hostname -I 2>/dev/null | awk '{print $1}' || true)"
fi
[[ -n $EGRESS_IP ]] || EGRESS_IP='127.0.0.1'
log "本机出口 IP: $EGRESS_IP"

# —— 3) 下载 os-api 二进制（版本感知升级；Web 前端 rust-embed 已内嵌于二进制，
#       无需单独拉取）——
# 升级判定取舍（三案对比，选 sha 对拍）：
#   a) sha256 对拍【选】——install.sh 生成时已烘焙源端分发件 sha256（零额外请求，
#      不下载即可判定"有无新版"）；局限：同版本重新编译 sha 也变 → 视为"构建刷新"
#      仍然自动替换（宁可多换一次，不可漏升级）。
#   b) 先下载到临时文件跑 --version 再决定——能拿到精确版本但每次执行都要全量
#      下载几十 MB，只为读一个版本号，浪费带宽且源端不可达时直接卡死。
#   c) dist 端点响应头携带版本——需改端点契约且老源端不回该头，兼容性差。
# 版本号只用于**提示**：确定要替换后，对临时文件跑 `--version` 与本地版本对比，
# 展示"升级 X→Y"（同版本重建提示构建刷新）。注意：老节点上**已安装的旧脚本**
# （0.1.4 生成的）没有本逻辑——首次升级仍需 --force；装上新脚本后今后升级全自动。
mkdir -p "$INSTALL_DIR"
NEED_DOWNLOAD=1
LOCAL_SHA=''
if [[ -x $BIN_PATH && $FORCE -ne 1 ]]; then
  LOCAL_SHA="$(sha256sum "$BIN_PATH" | awk '{print $1}')"
  if [[ -n $EXPECTED_SHA && "$LOCAL_SHA" == "$EXPECTED_SHA" ]]; then
    log "os-api 已是源端最新构建（$BIN_PATH, sha256=$LOCAL_SHA），跳过下载"
    NEED_DOWNLOAD=0
  elif [[ -z $EXPECTED_SHA ]]; then
    warn "源端分发件未就绪（install.sh 未烘焙 sha256），保持现有二进制（--force 可强制刷新）"
    NEED_DOWNLOAD=0
  else
    log "检测到新构建（本地 sha256=$LOCAL_SHA ≠ 源端 $EXPECTED_SHA），自动升级..."
  fi
fi
if [[ $NEED_DOWNLOAD -eq 1 ]]; then
  # 本地版本（提示用；跑不动 --version 的旧/异构二进制 → 空串 = 首装口径）
  LOCAL_VER=''
  if [[ -x $BIN_PATH ]]; then
    LOCAL_VER="$("$BIN_PATH" --version 2>/dev/null | head -n1 | awk '{print $NF}' || true)"
  fi
  TMP_FILE="$BIN_PATH.new.$$"
  log "检测到架构 $MACHINE_ARCH，从 $SRC 下载 $ARTIFACT ..."
  curl -fL --progress-bar "$SRC/api/v1/provisioning/dist/$ARTIFACT" -o "$TMP_FILE" \
    || die "下载失败: $SRC/api/v1/provisioning/dist/$ARTIFACT（x86_64: 源节点是否已 POST prepare-distributable？aarch64: 是否已跑 scripts/release.sh 刷新 dist/os-api-aarch64-latest？）"
  [[ "$(head -c 4 "$TMP_FILE")" == $'\x7fELF' ]] || { rm -f "$TMP_FILE"; die '下载内容不是 ELF 二进制，已中止'; }
  ACTUAL_SHA="$(sha256sum "$TMP_FILE" | awk '{print $1}')"
  if [[ -n $EXPECTED_SHA && "$ACTUAL_SHA" != "$EXPECTED_SHA" ]]; then
    rm -f "$TMP_FILE"
    die "sha256 不匹配: 期望 $EXPECTED_SHA, 实际 $ACTUAL_SHA"
  fi
  chmod 755 "$TMP_FILE"
  NEW_VER="$("$TMP_FILE" --version 2>/dev/null | head -n1 | awk '{print $NF}' || true)"
  mv -f "$TMP_FILE" "$BIN_PATH"
  if [[ -n $LOCAL_VER && -n $NEW_VER && "$LOCAL_VER" != "$NEW_VER" ]]; then
    log "升级 os-api：$LOCAL_VER → $NEW_VER（$BIN_PATH, sha256=$ACTUAL_SHA）"
  elif [[ -n $LOCAL_VER ]]; then
    log "os-api 构建已刷新（版本 ${NEW_VER:-?} 不变, $BIN_PATH, sha256=$ACTUAL_SHA）"
  else
    log "os-api 就绪（$BIN_PATH, v${NEW_VER:-未知}, sha256=$ACTUAL_SHA）"
  fi
fi

# —— 4) systemd 服务（整文件重写 + enable --now 天然幂等）——
# 更新源引导：NEXOS_UPDATE_REPO_URL 指向安装源节点的 NexHub git HTTP 通道
# （$SRC/git/nexos.git——新节点无 /tank 本地裸仓库，「更新」应用的 check 走
# git ls-remote --tags 纯网络查询，开箱即有可用更新源）。unit 整文件重写，
# 重复执行天然幂等更新该行（--source 换源后随 $SRC 跟随）。
NODE_NAME="${NAME:-$(hostname)}"
BOOTSTRAP_LIST="${BOOTSTRAP:-$NEXOS_BOOTSTRAP_DEFAULT}"
ADMIN_TOKEN="${TOKEN:-change-me-admin-token}"

cat > "$UNIT_PATH" <<UNIT
[Unit]
Description=NexOS API Gateway (os-api) — one-shot bootstrap install
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=$INSTALL_DIR
ExecStart=$BIN_PATH --addr 0.0.0.0:$PORT
Restart=always
RestartSec=3
Environment=NEXOS_ADMIN_TOKEN=$ADMIN_TOKEN
Environment=NEXOS_P2P_ENABLE=1
Environment=NEXOS_P2P_NAME=$NODE_NAME
Environment=NEXOS_P2P_BOOTSTRAP=$BOOTSTRAP_LIST
Environment=NEXOS_P2P_LISTEN=:7070
Environment=NEXOS_GIT_ADVERTISE_HOST=$EGRESS_IP
Environment=NEXOS_UPDATE_REPO_URL=$SRC/git/nexos.git
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
UNIT

command -v systemctl >/dev/null 2>&1 || die '未检测到 systemd（目标环境为 Ubuntu 22.04/24.04）'
# chroot 容忍（网络装机 autoinstall late-commands 场景）：curtin in-target 里
# 无运行中的 systemd（/run/systemd/system 缺席）——daemon-reload/restart 会
# "System has not been booted with systemd" 失败。此时只做离线 enable（符号
# 链级，chroot 内合法），服务由装后首次开机拉起；实体机上行为不变。
if [[ ! -e /run/systemd/system ]]; then
  log 'chroot 环境（autoinstall late-commands?）：离线 enable，服务首启由 systemd 拉起'
  systemctl enable "$SERVICE_NAME" >/dev/null 2>&1 || true
else
  systemctl daemon-reload
  # 升级路径必须显式 restart：`enable --now` 对已运行服务是空操作（幂等成功、
  # 不换进程）——Spark 实测踩坑：二进制升级后页面仍显示旧版本（旧进程在内存里
  # 跑旧 rust-embed 前端）。仅在二进制被替换过时 restart（首装/未变化不打扰）。
  if [[ $NEED_DOWNLOAD -eq 1 ]]; then
    log '二进制已更新，重启服务生效...'
    systemctl enable "$SERVICE_NAME" >/dev/null 2>&1 || true
    systemctl restart "$SERVICE_NAME" || die "服务重启失败（journalctl -u $SERVICE_NAME 排查）"
  else
    systemctl enable --now "$SERVICE_NAME" >/dev/null 2>&1 || systemctl restart "$SERVICE_NAME"
  fi
fi

# —— 5) 健康确认：等 P2P 自身份出现并摘取 NodeID ——
STATUS=''
for _ in $(seq 1 30); do
  STATUS="$(curl -sf "http://127.0.0.1:$PORT/api/v1/p2p/status" || true)"
  [[ -n $STATUS ]] && break
  sleep 1
done
NODE_ID="$(printf '%s' "$STATUS" | sed -n 's/.*"node_id":"\([^"]*\)".*/\1/p')"
[[ -n $NODE_ID ]] || NODE_ID='(获取失败, 查看 journalctl -u nexos-os-api)'
PEER_COUNT="$(printf '%s' "$STATUS" | sed -n 's/.*"peers_connected":\([0-9]*\).*/\1/p')"
[[ -n $PEER_COUNT ]] || PEER_COUNT='?'

cat <<SUMMARY

============================================================
  NexOS 安装完成
------------------------------------------------------------
  控制台:      http://$EGRESS_IP:$PORT
  NodeID:      $NODE_ID
  节点昵称:    $NODE_NAME
  P2P 引导:    $BOOTSTRAP_LIST
  已连节点数:  $PEER_COUNT
  Admin Token: $ADMIN_TOKEN   <- 默认值, 请尽快更换
------------------------------------------------------------
  集群确认: curl http://127.0.0.1:$PORT/api/v1/p2p/status
  服务日志: journalctl -u $SERVICE_NAME -f
============================================================
SUMMARY
"#;

/// 渲染一键安装脚本：嵌入安装源 / bootstrap 列表 / 双架构分发件 sha256
/// （脚本内 `uname -m` 分流后取本架构期望值做完整性对拍）。
#[must_use]
pub fn render_install_script(
    source_url: &str,
    bootstrap: &str,
    expected_sha256: &str,
    expected_sha256_aarch64: &str,
) -> String {
    INSTALL_SCRIPT_TEMPLATE
        .replace("@@SOURCE_URL@@", &strip_single_quotes(source_url))
        .replace("@@BOOTSTRAP@@", &strip_single_quotes(bootstrap))
        .replace(
            "@@EXPECTED_SHA256_AARCH64@@",
            &strip_single_quotes(expected_sha256_aarch64),
        )
        .replace("@@EXPECTED_SHA256@@", &strip_single_quotes(expected_sha256))
}

/// 构造一条 RouteSpec（component 固定 `provisioning`）。
fn spec(
    method: HttpMethod,
    path: &str,
    requires_auth: bool,
    required_roles: Vec<String>,
) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: "provisioning".to_string(),
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

/// 取 query 里的 `arch` 与 `platform` 参数（menu 端点用；缺省 None）。
fn query_params(path: &str) -> (Option<String>, Option<String>) {
    let mut arch = None;
    let mut platform = None;
    if let Some(q) = path.split_once('?').map(|(_, q)| q) {
        for pair in q.split('&') {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            match k {
                "arch" => arch = Some(v.to_string()),
                "platform" => platform = Some(v.to_string()),
                _ => {}
            }
        }
    }
    (arch, platform)
}

/// 种子条目 id（`26.04.1-amd64`）→ (version, arch, id)；白名单回构校验
/// （任何注入形态都 None——与 [`resolve_netboot_stream`] 同一道闸）。
fn parse_repo_seed_id(id: &str) -> Option<(String, String, String)> {
    let (version, arch) = id.rsplit_once('-')?;
    let triple = parse_repo_iso_filename(&format!("ubuntu-{version}-live-server-{arch}.iso"))?;
    if triple.2 != id {
        return None;
    }
    Some(triple)
}

/// 当前 ISO 时间戳（本地时区）。
fn now_iso() -> String {
    use chrono::Local;
    Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

// ----------------------------------------------------------------------------
// 默认示例数据
// ----------------------------------------------------------------------------

/// 默认示例 PXE 配置。
fn default_pxe_config() -> PxeConfig {
    PxeConfig {
        enabled: true,
        tftp_server: "10.0.0.1".into(),
        boot_mode: "uefi".into(),
        http_repo: "http://10.0.0.1:8080/provision".into(),
        default_bootfile: "ipxe.efi".into(),
    }
}

/// 默认示例启动条目（install + rescue）。
fn default_boot_entries() -> Vec<BootEntry> {
    vec![
        BootEntry {
            id: "install".into(),
            name: "OS 安装（阶段1）".into(),
            kernel: "vmlinuz".into(),
            initrd: "initrd.img".into(),
            cmdline:
                "base_image=http://10.0.0.1:8080/provision/base.squashfs install_disk=/dev/sda"
                    .into(),
            default_entry: true,
        },
        BootEntry {
            id: "rescue".into(),
            name: "救援模式".into(),
            kernel: "vmlinuz".into(),
            initrd: "initrd.img".into(),
            cmdline: "rescue=1".into(),
            default_entry: false,
        },
    ]
}

/// 预置 1 个 completed ISO 任务（让前端能演示产物下载）。**生产空表起步**
/// （v0.1.49 快赢 E.4 / 审查 B2-2：假 ISO 路径/假 sha256 是演示数据，不得
/// 出现在生产构造路径——真实数据铁律；照 api_gateway demo seed 先例
/// 「2026-08-30 删除的不是真实的数据」事故后全撤的同款 cfg 门）。仅
/// `#[cfg(test)]` 保留，供 iso_get_list_contains_demo 等行为契约测试。
fn demo_iso_tasks() -> Vec<IsoTask> {
    #[cfg(test)]
    {
        vec![IsoTask {
            id: "iso-1".into(),
            name: "OS Standard".into(),
            version: "0.1.0".into(),
            variant: "std".into(),
            arch: "x86_64".into(),
            ubuntu_version: "26.04".into(),
            status: "completed".into(),
            iso_path: Some("/build/iso/os-std-26.04-0.1.0.iso".into()),
            sha256: Some("abc123def4567890abcdef1234567890abcdef1234567890abcdef1234567890".into()),
            size_bytes: Some(850_000_000),
            created_at: "2026-08-08T08:00:00+08:00".into(),
            error: None,
            step: None,
            progress: None,
            build_log: Vec::new(),
        }]
    }
    #[cfg(not(test))]
    {
        Vec::new()
    }
}

/// 预置 1 个 SSH 目标（不预置 deploy 任务）。**生产空表起步**（v0.1.52
/// B 组审查附带发现：10.0.0.2 假 SSH 目标是演示数据，不得出现在生产构造
/// 路径——真实数据铁律；照 demo_iso_tasks v0.1.49 快赢 E.4 同款 cfg 门）。
/// 仅 `#[cfg(test)]` 保留，供 ssh_get_list_contains_demo 等行为契约测试。
fn demo_ssh_targets() -> Vec<SshTarget> {
    #[cfg(test)]
    {
        vec![SshTarget {
            id: "ssh-1".into(),
            name: "OS 节点 #1".into(),
            host: "10.0.0.2".into(),
            port: 22,
            user: "root".into(),
            private_key_path: Some("~/.ssh/id_ed25519".into()),
            status: "unknown".into(),
            last_checked: None,
            created_at: "2026-08-08T08:00:00+08:00".into(),
        }]
    }
    #[cfg(not(test))]
    {
        Vec::new()
    }
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt;

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

    /// 带自定义头的 GET（Host 头推导安装源用）。
    fn get_req_headers(path: &str, headers: serde_json::Value) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers,
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    /// 构造一个最小 IsoTask（供命名规则等纯函数测试）。
    fn mk_iso_task() -> IsoTask {
        IsoTask {
            id: "iso-1".into(),
            name: "OS Standard".into(),
            version: "1.0.0".into(),
            variant: "std".into(),
            arch: "x86_64".into(),
            ubuntu_version: "26.04".into(),
            status: "completed".into(),
            iso_path: None,
            sha256: None,
            size_bytes: None,
            created_at: "2026-08-08T08:00:00+08:00".into(),
            error: None,
            step: None,
            progress: None,
            build_log: Vec::new(),
        }
    }

    // ------------------------------------------------------------------
    // PATH 注入测试基建：独占锁 + 假 ssh/scp/mksquashfs/xorriso 脚本
    // （仓库既有 env 注入手法：std::env::set_var，本模块用全局锁串行化，
    //  避免 cargo test 并行线程间 PATH 互踩；所有涉及真实子进程 spawn 的
    //  测试必须在本锁内完成——包括轮询到终态，确保 spawn 全部落在假 PATH 窗口。）
    // ------------------------------------------------------------------
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// 独占执行闭包，期间 PATH = `dir` 前置 + 原系统 PATH 追加。
    ///
    /// 假 ssh/scp/mksquashfs 等优先命中（目录在前），脚本内部用到的系统工具
    /// （sleep/yes/head 等）仍可解析。闭包内用 `fake_rt` 把异步测试体
    /// block_on 在锁内跑完（含轮询到终态），确保所有子进程 spawn 都落在
    /// 假 PATH 窗口内；std MutexGuard 不跨 await（block_on 是同步调用），
    /// 不触发 await_holding_lock。
    fn with_fake_path<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var("PATH").unwrap_or_default();
        let combined = format!("{}:{}", dir.display(), old);
        std::env::set_var("PATH", &combined);
        let result = f();
        std::env::set_var("PATH", old);
        result
    }

    /// 独占模式：PATH 只含 `dir`（连系统工具都不可见）。
    ///
    /// 供"工具链缺失降级"测试用——保证 xorriso/mksquashfs 在任何机器上
    /// 都探测不到（不依赖本机是否装了 xorriso）。要求 dir 内脚本只用
    /// shell 内建命令。
    fn with_exclusive_path<T>(dir: &std::path::Path, f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", dir);
        let result = f();
        std::env::set_var("PATH", old);
        result
    }

    /// 单线程 runtime + block_on（配合 with_fake_path 在锁内驱动异步测试体）。
    fn fake_rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("测试 runtime 构建失败")
    }

    /// 写一个可执行假脚本（#!/bin/sh + body）。
    fn fake_bin(dir: &std::path::Path, name: &str, body: &str) {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "#!/bin/sh\n{body}").unwrap();
        drop(f);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("os-api-provisioning-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 轮询 GET deploy/:id 直到终态（20s 上限）。
    async fn poll_deploy_terminal(h: &ProvisioningRouteHandler, id: &str) -> DeployTask {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let resp = h
                .handle(get_req(&format!("/api/v1/provisioning/ssh/deploy/{id}")))
                .await
                .unwrap();
            assert_eq!(resp.status, 200, "deploy 轮询失败: {resp:?}");
            let t: DeployTask = serde_json::from_value(resp.body).unwrap();
            if t.status == "completed" || t.status == "failed" {
                return t;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("deploy {id} 未在期限内到达终态");
    }

    /// 轮询 GET iso/tasks/:id 直到终态（20s 上限）。
    async fn poll_iso_terminal(h: &ProvisioningRouteHandler, id: &str) -> IsoTask {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let resp = h
                .handle(get_req(&format!("/api/v1/provisioning/iso/tasks/{id}")))
                .await
                .unwrap();
            assert_eq!(resp.status, 200, "iso 轮询失败: {resp:?}");
            let t: IsoTask = serde_json::from_value(resp.body).unwrap();
            if t.status == "completed" || t.status == "failed" {
                return t;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("iso 任务 {id} 未在期限内到达终态");
    }

    /// 建一个可部署的假目标（走真实 POST 流程）。
    async fn add_target(h: &ProvisioningRouteHandler, host: &str, port: u16) -> String {
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/ssh/targets",
                serde_json::json!({"name": "t", "host": host, "port": port, "user": "root", "private_key_path": "/tmp/k"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        resp.body["id"].as_str().unwrap().to_string()
    }

    async fn deploy(
        h: &ProvisioningRouteHandler,
        target_id: &str,
        files: serde_json::Value,
        run_cmd: Option<&str>,
    ) -> ApiResponse {
        let mut body = serde_json::json!({"target_id": target_id, "files": files});
        if let Some(c) = run_cmd {
            body["run_cmd"] = serde_json::json!(c);
        }
        h.handle(post_req("/api/v1/provisioning/ssh/deploy", body))
            .await
            .unwrap()
    }

    // ============ PXE 子项测试（从 pxe.rs 搬移改造）============

    // —— GET /api/v1/provisioning/pxe/config ——
    #[tokio::test]
    async fn pxe_get_config_returns_default() {
        let h = ProvisioningRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/provisioning/pxe/config"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["enabled"], true);
        assert_eq!(resp.body["tftp_server"], "10.0.0.1");
        assert_eq!(resp.body["boot_mode"], "uefi");
        assert_eq!(resp.body["default_bootfile"], "ipxe.efi");
    }

    // —— POST /api/v1/provisioning/pxe/boot-entries ——
    #[tokio::test]
    async fn pxe_post_boot_entry_creates_201() {
        let h = ProvisioningRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/pxe/boot-entries",
                serde_json::json!({
                    "id": "custom",
                    "name": "自定义内核",
                    "kernel": "vmlinuz",
                    "initrd": "initrd.img",
                    "cmdline": "debug=1",
                    "default": false,
                }),
            ))
            .await
            .expect("create 应成功");
        assert_eq!(resp.status, 201);
        assert_eq!(resp.body["id"], "custom");
        assert_eq!(h.boot_entries_snapshot().len(), 1);
    }

    // —— 新 default 条目会清掉旧 default（单锁作用域内完成）——
    #[tokio::test]
    async fn pxe_post_default_entry_clears_previous_default() {
        let h = ProvisioningRouteHandler::new();
        assert_eq!(h.boot_entries_snapshot()[0].id, "install");
        assert!(h.boot_entries_snapshot()[0].default_entry);
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/pxe/boot-entries",
                serde_json::json!({
                    "id": "newdef",
                    "name": "新默认",
                    "kernel": "vmlinuz",
                    "initrd": "initrd.img",
                    "cmdline": "",
                    "default": true,
                }),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        let entries = h.boot_entries_snapshot();
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries.iter().filter(|e| e.default_entry).count(),
            1,
            "default 唯一"
        );
        assert!(
            entries
                .iter()
                .find(|e| e.id == "newdef")
                .unwrap()
                .default_entry
        );
        assert!(
            !entries
                .iter()
                .find(|e| e.id == "install")
                .unwrap()
                .default_entry
        );
    }

    // —— GET /api/v1/provisioning/pxe/status ——
    #[tokio::test]
    async fn pxe_status_initial_is_stopped() {
        let h = ProvisioningRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/provisioning/pxe/status"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["running"], false);
        assert_eq!(resp.body["state"], "stopped");
    }

    // —— POST /api/v1/provisioning/pxe/start ——
    #[tokio::test]
    async fn pxe_start_sets_running_true() {
        let h = ProvisioningRouteHandler::new();
        assert!(!h.pxe_running_snapshot());
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/pxe/start",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["running"], true);
        assert_eq!(resp.body["state"], "running");
        assert!(h.pxe_running_snapshot());
    }

    // ============ ISO 子项测试 ============

    // —— iso_filename 命名规则 ——
    #[test]
    fn iso_filename_rule() {
        let task = mk_iso_task();
        assert_eq!(iso_filename(&task), "os-std-26.04-1.0.0.iso");
        // clone 变体
        let mut clone_task = task.clone();
        clone_task.variant = "clone".into();
        assert_eq!(iso_filename(&clone_task), "os-clone-26.04-1.0.0.iso");
    }

    // —— POST /api/v1/provisioning/iso/tasks 建 status=pending ——
    #[tokio::test]
    async fn iso_post_task_creates_pending() {
        let h = ProvisioningRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/iso/tasks",
                serde_json::json!({"name": "测试 ISO"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "create body: {resp:?}");
        assert_eq!(resp.body["status"], "pending");
        // 缺省值
        assert_eq!(resp.body["version"], "0.1.0");
        assert_eq!(resp.body["variant"], "std");
        assert_eq!(resp.body["arch"], "x86_64");
        assert_eq!(resp.body["ubuntu_version"], "26.04");
        // 未触发构建（iso_path 为 null）
        assert!(resp.body["iso_path"].is_null());
        assert!(resp.body["build_log"].as_array().unwrap().is_empty());
    }

    // —— GET /api/v1/provisioning/iso/tasks 列表含示例 ——
    #[tokio::test]
    async fn iso_get_list_contains_demo() {
        let h = ProvisioningRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/provisioning/iso/tasks"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], "iso-1");
        assert_eq!(arr[0]["name"], "OS Standard");
        assert_eq!(arr[0]["status"], "completed");
        assert_eq!(arr[0]["iso_path"], "/build/iso/os-std-26.04-0.1.0.iso");
    }

    // —— DELETE /api/v1/provisioning/iso/tasks/:id ——
    #[tokio::test]
    async fn iso_delete_task_removes() {
        let h = ProvisioningRouteHandler::new();
        let resp = h
            .handle(del_req("/api/v1/provisioning/iso/tasks/iso-1"))
            .await
            .unwrap();
        assert_eq!(resp.status, 204);
        assert!(h.iso_tasks_snapshot().is_empty());
        // 再删同一个 → 404
        let resp = h
            .handle(del_req("/api/v1/provisioning/iso/tasks/iso-1"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
    }

    // —— POST /iso/tasks/:id/build：任务不存在 → 404 ——
    #[tokio::test]
    async fn iso_build_unknown_task_404() {
        let h = ProvisioningRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/iso/tasks/nope/build",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);
        assert!(resp.body["error"]
            .as_str()
            .unwrap()
            .contains("ISO 任务不存在"));
    }

    // —— POST /iso/tasks/:id/build：工具链缺失 → failed 附安装指引 ——
    #[test]
    fn iso_build_without_toolchain_fails_with_guidance() {
        let dir = temp_dir("iso-noguide");
        // 独占 PATH（空目录）：任何机器上 xorriso/mksquashfs 都探测不到
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        with_exclusive_path(&dir.join("bin"), || {
            fake_rt().block_on(async {
                let h = ProvisioningRouteHandler::with_options(
                    dir.join("iso-out"),
                    DEPLOY_FILE_TIMEOUT,
                    DEPLOY_CMD_TIMEOUT,
                );
                *h.iso_tasks.lock().unwrap() = Vec::new();
                let created = h
                    .handle(post_req(
                        "/api/v1/provisioning/iso/tasks",
                        serde_json::json!({"name": "n"}),
                    ))
                    .await
                    .unwrap();
                let id = created.body["id"].as_str().unwrap().to_string();

                let resp = h
                    .handle(post_req(
                        &format!("/api/v1/provisioning/iso/tasks/{id}/build"),
                        serde_json::Value::Null,
                    ))
                    .await
                    .unwrap();
                assert_eq!(resp.status, 200, "body: {resp:?}");
                assert_eq!(resp.body["status"], "failed");
                let err = resp.body["error"].as_str().unwrap();
                assert!(err.contains("xorriso"), "指引应点名缺失工具: {err}");
                assert!(err.contains("apt install"), "指引应给安装命令: {err}");
                // 日志里也留了指引
                let t = poll_iso_terminal(&h, &id).await;
                assert_eq!(t.status, "failed");
                assert!(t.build_log.iter().any(|l| l.contains("xorriso")));
            })
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    // —— POST /iso/tasks/:id/build：真实子进程构建（假工具链）→ completed ——
    #[test]
    fn iso_build_real_subprocess_completes() {
        let dir = temp_dir("iso-real");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        fake_bin(&bin, "mksquashfs", "exit 0");
        // xorriso：解析 -o 后的产物路径并真实落盘（供 file_size/sha256 阶段使用）
        fake_bin(
            &bin,
            "xorriso",
            r#"out=""; prev=""; for a in "$@"; do if [ "$prev" = "-o" ]; then out="$a"; fi; prev="$a"; done; if [ -n "$out" ]; then printf 'ISO-CONTENT' > "$out"; fi; exit 0"#,
        );
        fake_bin(
            &bin,
            "sha256sum",
            r#"echo "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  $1""#,
        );
        let out_root = dir.join("iso-out");

        with_fake_path(&bin, || {
            fake_rt().block_on(async {
                let h = ProvisioningRouteHandler::with_options(
                    out_root.clone(),
                    DEPLOY_FILE_TIMEOUT,
                    DEPLOY_CMD_TIMEOUT,
                );
                *h.iso_tasks.lock().unwrap() = Vec::new();
                let created = h
                    .handle(post_req(
                        "/api/v1/provisioning/iso/tasks",
                        serde_json::json!({"name": "real", "version": "1.2.3"}),
                    ))
                    .await
                    .unwrap();
                let id = created.body["id"].as_str().unwrap().to_string();

                let resp = h
                    .handle(post_req(
                        &format!("/api/v1/provisioning/iso/tasks/{id}/build"),
                        serde_json::Value::Null,
                    ))
                    .await
                    .unwrap();
                assert_eq!(resp.status, 200, "body: {resp:?}");
                assert_eq!(resp.body["status"], "building", "body: {resp:?}");

                let t = poll_iso_terminal(&h, &id).await;
                assert_eq!(
                    t.status, "completed",
                    "error: {:?} log: {:?}",
                    t.error, t.build_log
                );
                let iso_path = t.iso_path.expect("completed 应有产物路径");
                assert!(iso_path.starts_with(out_root.to_str().unwrap()));
                assert!(iso_path.ends_with(".iso"));
                assert!(std::path::Path::new(&iso_path).exists(), "产物应真实落盘");
                assert_eq!(
                    t.sha256.as_deref(),
                    Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
                );
                assert_eq!(t.size_bytes, Some(11), "fake xorriso 写入 11 字节");
                // 构建日志记录了每步真实子进程
                let joined = t.build_log.join("\n");
                assert!(joined.contains("$ mksquashfs"), "日志: {joined}");
                assert!(joined.contains("$ xorriso"), "日志: {joined}");
                assert!(joined.contains("$ sha256sum"), "日志: {joined}");
            })
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    // —— POST /iso/tasks/:id/build：构建中重复触发 → 409 ——
    #[test]
    fn iso_build_conflict_while_building() {
        let dir = temp_dir("iso-409");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        fake_bin(&bin, "mksquashfs", "exit 0");
        // 拖长构建窗口 + 落盘产物（file_size 阶段需要真实文件）
        fake_bin(
            &bin,
            "xorriso",
            r#"out=""; prev=""; for a in "$@"; do if [ "$prev" = "-o" ]; then out="$a"; fi; prev="$a"; done; sleep 1; if [ -n "$out" ]; then printf 'ISO-CONTENT' > "$out"; fi; exit 0"#,
        );
        fake_bin(
            &bin,
            "sha256sum",
            r#"echo "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  $1""#,
        );

        with_fake_path(&bin, || {
            fake_rt().block_on(async {
                let h = ProvisioningRouteHandler::with_options(
                    dir.join("iso-out"),
                    DEPLOY_FILE_TIMEOUT,
                    DEPLOY_CMD_TIMEOUT,
                );
                *h.iso_tasks.lock().unwrap() = Vec::new();
                let created = h
                    .handle(post_req(
                        "/api/v1/provisioning/iso/tasks",
                        serde_json::json!({"name": "x"}),
                    ))
                    .await
                    .unwrap();
                let id = created.body["id"].as_str().unwrap().to_string();

                let first = h
                    .handle(post_req(
                        &format!("/api/v1/provisioning/iso/tasks/{id}/build"),
                        serde_json::Value::Null,
                    ))
                    .await
                    .unwrap();
                assert_eq!(first.status, 200);

                let second = h
                    .handle(post_req(
                        &format!("/api/v1/provisioning/iso/tasks/{id}/build"),
                        serde_json::Value::Null,
                    ))
                    .await
                    .unwrap();
                assert_eq!(second.status, 409, "构建中应拒绝: {second:?}");

                let t = poll_iso_terminal(&h, &id).await;
                assert_eq!(t.status, "completed");
                // 完成后再触发 → 仍 409（已存在产物）
                let third = h
                    .handle(post_req(
                        &format!("/api/v1/provisioning/iso/tasks/{id}/build"),
                        serde_json::Value::Null,
                    ))
                    .await
                    .unwrap();
                assert_eq!(third.status, 409);
            })
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    // —— 非法架构任务构建 → failed（spec 派生校验）——
    #[test]
    fn iso_build_bad_arch_fails() {
        let dir = temp_dir("iso-arch");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        fake_bin(&bin, "mksquashfs", "exit 0");
        fake_bin(&bin, "xorriso", "exit 0");
        fake_bin(
            &bin,
            "sha256sum",
            r#"echo "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  $1""#,
        );
        with_fake_path(&bin, || {
            fake_rt().block_on(async {
                let h = ProvisioningRouteHandler::with_options(
                    dir.join("iso-out"),
                    DEPLOY_FILE_TIMEOUT,
                    DEPLOY_CMD_TIMEOUT,
                );
                *h.iso_tasks.lock().unwrap() = Vec::new();
                let created = h
                    .handle(post_req(
                        "/api/v1/provisioning/iso/tasks",
                        serde_json::json!({"name": "bad", "arch": "mips"}),
                    ))
                    .await
                    .unwrap();
                let id = created.body["id"].as_str().unwrap().to_string();
                let resp = h
                    .handle(post_req(
                        &format!("/api/v1/provisioning/iso/tasks/{id}/build"),
                        serde_json::Value::Null,
                    ))
                    .await
                    .unwrap();
                assert_eq!(resp.status, 200);
                assert_eq!(resp.body["status"], "failed");
                assert!(resp.body["error"].as_str().unwrap().contains("架构"));
            })
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ============ SSH 子项测试 ============

    // —— POST /api/v1/provisioning/ssh/targets 添加 ——
    #[tokio::test]
    async fn ssh_post_target_creates() {
        let h = ProvisioningRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/ssh/targets",
                serde_json::json!({"name": "新节点", "host": "192.168.1.50"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "create body: {resp:?}");
        assert_eq!(resp.body["port"], 22, "port 默认 22");
        assert_eq!(resp.body["user"], "root", "user 默认 root");
        assert_eq!(resp.body["status"], "unknown");
        assert!(
            resp.body["private_key_path"].is_null(),
            "未传 key 应为 null"
        );
        // 无 password 字段（红线）
        assert!(
            resp.body.get("password").is_none(),
            "SSH 目标不得有 password 字段"
        );
    }

    // —— GET /api/v1/provisioning/ssh/targets 列表 ——
    #[tokio::test]
    async fn ssh_get_list_contains_demo() {
        let h = ProvisioningRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/provisioning/ssh/targets"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["id"], "ssh-1");
        assert_eq!(arr[0]["host"], "10.0.0.2");
        assert_eq!(arr[0]["port"], 22);
    }

    // —— test 路由声明（不真连，只验路由匹配 + 返回结构）——
    #[tokio::test]
    async fn ssh_test_route_matches_and_returns_target() {
        // 用一个不存在的目标验证路由匹配（应返回 404 body 而非兜底 404，
        // 且不 panic 不报错）—— 证明 :id/test 路由确实被命中。
        let h = ProvisioningRouteHandler::new();
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/ssh/targets/nonexistent/test",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        // 命中 test 路由但目标不存在 → 404 body（含 error 字段）
        assert_eq!(resp.status, 404);
        assert!(
            resp.body["error"]
                .as_str()
                .unwrap()
                .contains("SSH 目标不存在"),
            "应命中 test 路由返回目标不存在: {resp:?}"
        );
        // 验证 test 路由在 routes() 中声明
        let routes = h.routes().await;
        assert!(
            routes.iter().any(|r| r.method == HttpMethod::Post
                && r.path == "/api/v1/provisioning/ssh/targets/:id/test"),
            "test 路由必须声明"
        );
    }

    // ============ SSH deploy 真实执行测试（PATH 注入假 ssh/scp）============

    // —— 全流程成功：mkdir + 逐文件 scp + run_cmd，参数形态与结果断言 ——
    #[test]
    fn ssh_deploy_real_success_records_every_step() {
        let dir = temp_dir("dep-ok");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let log = dir.join("invocations.log");
        let log_str = log.to_str().unwrap();
        fake_bin(
            &bin,
            "ssh",
            &format!(r#"echo "ssh $*" >> {log_str}; exit 0"#),
        );
        fake_bin(
            &bin,
            "scp",
            &format!(r#"echo "scp $*" >> {log_str}; exit 0"#),
        );

        with_fake_path(&bin, || {
            fake_rt().block_on(async {
                let h = ProvisioningRouteHandler::with_options(
                    dir.join("iso"),
                    DEPLOY_FILE_TIMEOUT,
                    DEPLOY_CMD_TIMEOUT,
                );
                *h.ssh_targets.lock().unwrap() = Vec::new();
                let tid = add_target(&h, "10.9.9.9", 2222).await;

                let resp = deploy(
                    &h,
                    &tid,
                    serde_json::json!([
                        {"local_path": "/tmp/agent", "remote_path": "/usr/local/bin/agent"},
                        {"local_path": "/tmp/cfg", "remote_path": "/etc/app/cfg.toml"}
                    ]),
                    Some("systemctl restart agent"),
                )
                .await;
                assert_eq!(resp.status, 201, "body: {resp:?}");
                assert_eq!(resp.body["status"], "pending");
                let id = resp.body["id"].as_str().unwrap().to_string();

                let t = poll_deploy_terminal(&h, &id).await;
                assert_eq!(t.status, "completed", "error: {:?} results: {:?}", t.error, t.results);
                assert!(t.error.is_none());
                assert!(t.started_at.is_some());
                assert!(t.finished_at.is_some());

                // 文件级结果：两个 success、有耗时
                assert_eq!(t.results.len(), 2);
                for r in &t.results {
                    assert_eq!(r.status, "success", "{r:?}");
                    assert_eq!(r.exit_code, Some(0));
                    assert!(r.duration_ms.is_some(), "耗时应记录: {r:?}");
                }

                // run_cmd 结果：exit 0（duration_ms 为非可选字段，能读到即已记录）
                let cmd = t.cmd_output.expect("应记录 cmd_output");
                assert_eq!(cmd.exit_code, 0);
                let _ = cmd.duration_ms;

                // 真实子进程参数形态（关键安全/契约断言）
                let invocations = std::fs::read_to_string(&log).unwrap();
                // mkdir：BatchMode + accept-new + -p 端口 + -i 密钥 + 两个目录一次建
                assert!(
                    invocations.contains("ssh -o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new -p 2222 -i /tmp/k root@10.9.9.9"),
                    "ssh 参数形态: {invocations}"
                );
                assert!(
                    invocations.contains("mkdir -p '/etc/app' '/usr/local/bin'"),
                    "远端目录排序去重+引号包裹: {invocations}"
                );
                // scp：大写 -P + 源/目的
                assert!(
                    invocations.contains("scp -o BatchMode=yes -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new -P 2222 -i /tmp/k /tmp/agent root@10.9.9.9:/usr/local/bin/agent"),
                    "scp 参数形态: {invocations}"
                );
                assert!(invocations.contains("/tmp/cfg root@10.9.9.9:/etc/app/cfg.toml"));
                // run_cmd：sh -c 单引号包裹（远端整体一条命令）
                assert!(
                    invocations.contains("sh -c 'systemctl restart agent'"),
                    "run_cmd 应经 sh -c 引用包裹: {invocations}"
                );
            })
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    // —— scp 失败：该文件 failed + 后续 skipped + 任务 failed ——
    #[test]
    fn ssh_deploy_scp_failure_fails_and_skips_rest() {
        let dir = temp_dir("dep-scpfail");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        fake_bin(&bin, "ssh", "exit 0");
        fake_bin(&bin, "scp", "echo boom >&2; exit 1");

        with_fake_path(&bin, || {
            fake_rt().block_on(async {
                let h = ProvisioningRouteHandler::with_options(
                    dir.join("iso"),
                    DEPLOY_FILE_TIMEOUT,
                    DEPLOY_CMD_TIMEOUT,
                );
                *h.ssh_targets.lock().unwrap() = Vec::new();
                let tid = add_target(&h, "10.0.0.2", 22).await;

                let resp = deploy(
                    &h,
                    &tid,
                    serde_json::json!([
                        {"local_path": "/tmp/a", "remote_path": "/tmp/a"},
                        {"local_path": "/tmp/b", "remote_path": "/tmp/b"}
                    ]),
                    Some("true"),
                )
                .await;
                assert_eq!(resp.status, 201);
                let id = resp.body["id"].as_str().unwrap().to_string();

                let t = poll_deploy_terminal(&h, &id).await;
                assert_eq!(t.status, "failed");
                let err = t.error.unwrap();
                assert!(err.contains("/tmp/a"), "错误应点名失败文件: {err}");
                assert_eq!(t.results[0].status, "failed");
                assert_eq!(t.results[0].exit_code, Some(1));
                assert!(t.results[0].error.as_deref().unwrap().contains("boom"));
                assert_eq!(t.results[1].status, "skipped", "后续文件应 skipped");
                assert!(t.cmd_output.is_none(), "文件失败不应再跑 run_cmd");
            })
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    // —— mkdir 失败：全部文件 failed（未尝试传输）——
    #[test]
    fn ssh_deploy_mkdir_failure_fails_all() {
        let dir = temp_dir("dep-mkdirfail");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        fake_bin(&bin, "ssh", "echo nope >&2; exit 3");
        fake_bin(&bin, "scp", "exit 0");

        with_fake_path(&bin, || {
            fake_rt().block_on(async {
                let h = ProvisioningRouteHandler::with_options(
                    dir.join("iso"),
                    DEPLOY_FILE_TIMEOUT,
                    DEPLOY_CMD_TIMEOUT,
                );
                *h.ssh_targets.lock().unwrap() = Vec::new();
                let tid = add_target(&h, "10.0.0.3", 22).await;
                let resp = deploy(
                    &h,
                    &tid,
                    serde_json::json!([{"local_path": "/tmp/a", "remote_path": "/x/y/a"}]),
                    None,
                )
                .await;
                let id = resp.body["id"].as_str().unwrap().to_string();
                let t = poll_deploy_terminal(&h, &id).await;
                assert_eq!(t.status, "failed");
                assert!(t.error.unwrap().contains("远程目录创建失败"));
                assert_eq!(t.results[0].status, "failed");
            })
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    // —— run_cmd 非零退出：任务 failed + cmd_output 记录退出码与输出 ——
    #[test]
    fn ssh_deploy_run_cmd_failure_captures_output() {
        let dir = temp_dir("dep-cmdfail");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // mkdir 等普通 ssh 调用成功；sh -c 的 run_cmd 退出 7
        fake_bin(
            &bin,
            "ssh",
            r#"for last; do :; done; case "$last" in "sh -c"*) echo out-line; echo cmd-bad >&2; exit 7;; *) exit 0;; esac"#,
        );
        fake_bin(&bin, "scp", "exit 0");

        with_fake_path(&bin, || {
            fake_rt().block_on(async {
                let h = ProvisioningRouteHandler::with_options(
                    dir.join("iso"),
                    DEPLOY_FILE_TIMEOUT,
                    DEPLOY_CMD_TIMEOUT,
                );
                *h.ssh_targets.lock().unwrap() = Vec::new();
                let tid = add_target(&h, "10.0.0.4", 22).await;
                let resp = deploy(
                    &h,
                    &tid,
                    serde_json::json!([{"local_path": "/tmp/a", "remote_path": "/tmp/a"}]),
                    Some("false"),
                )
                .await;
                let id = resp.body["id"].as_str().unwrap().to_string();

                let t = poll_deploy_terminal(&h, &id).await;
                assert_eq!(t.status, "failed");
                assert!(t.error.unwrap().contains("远程命令失败"));
                assert_eq!(t.results[0].status, "success", "文件传输应已成功");
                let cmd = t.cmd_output.unwrap();
                assert_eq!(cmd.exit_code, 7);
                assert_eq!(cmd.stdout.trim(), "out-line");
                assert!(cmd.stderr.contains("cmd-bad"));
            })
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    // —— 同目标互斥：进行中 409（附 deploy_id），完成后可再部署 ——
    #[test]
    fn ssh_deploy_mutual_exclusion_per_target() {
        let dir = temp_dir("dep-mutex");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        fake_bin(&bin, "ssh", "sleep 0.5; exit 0");
        fake_bin(&bin, "scp", "sleep 0.5; exit 0");

        with_fake_path(&bin, || {
            fake_rt().block_on(async {
                let h = ProvisioningRouteHandler::with_options(
                    dir.join("iso"),
                    DEPLOY_FILE_TIMEOUT,
                    DEPLOY_CMD_TIMEOUT,
                );
                *h.ssh_targets.lock().unwrap() = Vec::new();
                let t1 = add_target(&h, "10.0.0.5", 22).await;
                let t2 = add_target(&h, "10.0.0.6", 22).await;

                let first = deploy(
                    &h,
                    &t1,
                    serde_json::json!([{"local_path": "/tmp/a", "remote_path": "/tmp/a"}]),
                    None,
                )
                .await;
                assert_eq!(first.status, 201);
                let first_id = first.body["id"].as_str().unwrap().to_string();

                // 同目标并发 → 409 + 在跑任务 id
                let dup = deploy(
                    &h,
                    &t1,
                    serde_json::json!([{"local_path": "/tmp/b", "remote_path": "/tmp/b"}]),
                    None,
                )
                .await;
                assert_eq!(dup.status, 409, "body: {dup:?}");
                assert_eq!(dup.body["deploy_id"], first_id);

                // 不同目标不受影响
                let other = deploy(
                    &h,
                    &t2,
                    serde_json::json!([{"local_path": "/tmp/c", "remote_path": "/tmp/c"}]),
                    None,
                )
                .await;
                assert_eq!(other.status, 201);

                // 等第一个完成 → 互斥释放，可再次部署
                let done = poll_deploy_terminal(&h, &first_id).await;
                assert_eq!(done.status, "completed");
                let again = deploy(
                    &h,
                    &t1,
                    serde_json::json!([{"local_path": "/tmp/d", "remote_path": "/tmp/d"}]),
                    None,
                )
                .await;
                assert_eq!(again.status, 201, "完成后互斥应释放");
            })
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    // —— 单文件传输超时：kill_on_drop 强杀 + failed 附超时说明 ——
    #[test]
    fn ssh_deploy_file_transfer_timeout() {
        let dir = temp_dir("dep-timeout");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        fake_bin(&bin, "ssh", "exit 0");
        fake_bin(&bin, "scp", "sleep 3; exit 0");

        with_fake_path(&bin, || {
            fake_rt().block_on(async {
                let h = ProvisioningRouteHandler::with_options(
                    dir.join("iso"),
                    Duration::from_millis(300), // 文件传输 300ms 超时
                    DEPLOY_CMD_TIMEOUT,
                );
                *h.ssh_targets.lock().unwrap() = Vec::new();
                let tid = add_target(&h, "10.0.0.7", 22).await;
                let resp = deploy(
                    &h,
                    &tid,
                    serde_json::json!([{"local_path": "/tmp/slow", "remote_path": "/tmp/slow"}]),
                    None,
                )
                .await;
                let id = resp.body["id"].as_str().unwrap().to_string();

                let started = Instant::now();
                let t = poll_deploy_terminal(&h, &id).await;
                // 3s 的假 scp 必须 ~300ms 就被判超时（否则没走 timeout 分支）
                assert!(
                    started.elapsed() < Duration::from_secs(2),
                    "应快速超时，实际 {:?}",
                    started.elapsed()
                );
                assert_eq!(t.status, "failed");
                assert!(t.error.unwrap().contains("超时"));
                assert_eq!(t.results[0].status, "failed");
                assert!(t.results[0].error.as_deref().unwrap().contains("超时"));
            })
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    // —— 输出捕获 8KB 截断 ——
    #[test]
    fn ssh_deploy_output_capped_8kb() {
        let dir = temp_dir("dep-cap");
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // run_cmd 打 20KB stdout + 10KB stderr（yes|head 管道，POSIX sh 通用）
        fake_bin(
            &bin,
            "ssh",
            r#"for last; do :; done; case "$last" in "sh -c"*) yes xxxxx | head -c 20480; yes yyyyy | head -c 10240 >&2; exit 0;; *) exit 0;; esac"#,
        );
        fake_bin(&bin, "scp", "exit 0");

        with_fake_path(&bin, || {
            fake_rt().block_on(async {
                let h = ProvisioningRouteHandler::with_options(
                    dir.join("iso"),
                    DEPLOY_FILE_TIMEOUT,
                    DEPLOY_CMD_TIMEOUT,
                );
                *h.ssh_targets.lock().unwrap() = Vec::new();
                let tid = add_target(&h, "10.0.0.8", 22).await;
                let resp = deploy(&h, &tid, serde_json::json!([]), Some("cat /dev/zero")).await;
                assert_eq!(resp.status, 201);
                let id = resp.body["id"].as_str().unwrap().to_string();
                let t = poll_deploy_terminal(&h, &id).await;
                assert_eq!(t.status, "completed");
                let cmd = t.cmd_output.unwrap();
                assert!(
                    cmd.stdout.len() <= DEPLOY_OUTPUT_CAP + 64,
                    "stdout 应截断: {}",
                    cmd.stdout.len()
                );
                assert!(cmd.stdout.contains("截断"), "应带截断标记");
                assert!(cmd.stderr.len() <= DEPLOY_OUTPUT_CAP + 64);
            })
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    // —— 校验：files 与 run_cmd 至少一项 → 400 ——
    #[tokio::test]
    async fn ssh_deploy_requires_files_or_cmd() {
        let h = ProvisioningRouteHandler::with_empty();
        let resp = deploy(&h, "ssh-x", serde_json::json!([]), None).await;
        // 目标不存在先撞 404 还是参数先 400？——参数校验在前
        assert_eq!(resp.status, 400, "body: {resp:?}");
        assert!(resp.body["error"]
            .as_str()
            .unwrap()
            .contains("至少提供一项"));
    }

    // —— 校验：目标不存在 → 404 ——
    #[tokio::test]
    async fn ssh_deploy_unknown_target_404() {
        let h = ProvisioningRouteHandler::with_empty();
        let resp = deploy(
            &h,
            "ghost",
            serde_json::json!([{"local_path": "/a", "remote_path": "/b"}]),
            None,
        )
        .await;
        assert_eq!(resp.status, 404);
        assert!(resp.body["error"]
            .as_str()
            .unwrap()
            .contains("SSH 目标不存在"));
    }

    // —— 统一任务框架双写（v0.1.54）——
    //
    // iso：建任务即登记 queued；deploy：创建 queued → 运行 running → 终态
    // error（127.0.0.1:1 连接拒绝，秒级失败——不依赖真实网络/ssh 可用性）。

    fn unified_store_at(tag: &str) -> Arc<crate::handlers::tasks_common::UnifiedTaskStore> {
        let dir = std::env::temp_dir().join(format!(
            "nexos-prov-unified-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(crate::handlers::tasks_common::UnifiedTaskStore::with_path(
            dir.join("tasks.db").to_str().unwrap(),
        ))
    }

    #[tokio::test]
    async fn unified_dual_write_iso_creation_registers_queued() {
        let unified = unified_store_at("iso");
        let h = ProvisioningRouteHandler::with_empty().with_unified_tasks(Arc::clone(&unified));
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/iso/tasks",
                serde_json::json!({"name": "统一测试 ISO"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "{resp:?}");
        let rows = unified.list(Some("iso"), None, 50);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].status, crate::handlers::tasks_common::UnifiedStatus::Queued);
        assert!(rows[0].label.contains("统一测试 ISO"), "label 含任务名");
        assert!(rows[0].finished_at.is_none(), "未终态");
        // 未注入 → 零旁路（建任务不写 unified）
        let h2 = ProvisioningRouteHandler::with_empty();
        let resp = h2
            .handle(post_req(
                "/api/v1/provisioning/iso/tasks",
                serde_json::json!({"name": "无登记 ISO"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201);
        assert!(unified.list(Some("iso"), None, 50).len() == 1, "未注入不双写");
    }

    #[tokio::test]
    async fn unified_dual_write_deploy_lifecycle_visible() {
        let unified = unified_store_at("deploy");
        // 短超时（连接拒绝秒级失败；防环境 ssh 语义差异拖慢测试）
        let h = ProvisioningRouteHandler::with_options(
            std::path::PathBuf::from("./build/iso-unified-test"),
            Duration::from_millis(800),
            Duration::from_millis(800),
        )
        .with_unified_tasks(Arc::clone(&unified));
        // 建目标（127.0.0.1:1 不可达）
        let resp = h
            .handle(post_req(
                "/api/v1/provisioning/ssh/targets",
                serde_json::json!({"name": "黑洞", "host": "127.0.0.1", "port": 1}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 201, "{resp:?}");
        let target_id = resp.body["id"].as_str().unwrap().to_string();

        let resp = deploy(&h, &target_id, serde_json::json!([]), Some("echo hi")).await;
        assert_eq!(resp.status, 201, "{resp:?}");
        let deploy_id = resp.body["id"].as_str().unwrap().to_string();
        // 创建即登记（queued 或已推进 running——不锁中间态）
        let mut saw_row = false;
        for _ in 0..50 {
            let rows = unified.list(Some("deploy"), None, 50);
            if let Some(r) = rows.iter().find(|r| r.id == deploy_id) {
                saw_row = true;
                if matches!(
                    r.status,
                    crate::handlers::tasks_common::UnifiedStatus::Error
                ) {
                    assert!(r.error.is_some(), "失败原因落表: {r:?}");
                    assert!(r.finished_at.is_some(), "终态补记");
                    assert!(r.label.contains("黑洞"), "label 含目标名");
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(
            saw_row,
            "deploy 任务应在 unified_tasks 可见（id={deploy_id}）"
        );
        panic!("deploy 任务 5s 内未到终态（连接拒绝应秒级失败）");
    }

    // —— 列表端点：最新在前 + 终态任务可见 ——
    #[tokio::test]
    async fn ssh_deploy_list_latest_first() {
        let h = ProvisioningRouteHandler::with_empty();
        {
            let mut tasks = h.deploy_tasks.lock().unwrap();
            for (id, status) in [("deploy-1", "completed"), ("deploy-2", "failed")] {
                tasks.push(DeployTask {
                    id: id.into(),
                    target_id: "ssh-1".into(),
                    files: vec![],
                    run_cmd: None,
                    status: status.into(),
                    created_at: now_iso(),
                    error: None,
                    results: vec![],
                    cmd_output: None,
                    started_at: None,
                    finished_at: None,
                });
            }
        }
        let resp = h
            .handle(get_req("/api/v1/provisioning/ssh/deploys"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let arr = resp.body.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["id"], "deploy-2", "最新在前");
        assert_eq!(arr[1]["id"], "deploy-1");
    }

    // ============ 纯函数单测 ============

    #[test]
    fn sh_quote_wraps_single_quotes() {
        assert_eq!(sh_quote("simple"), "'simple'");
        assert_eq!(sh_quote("it's"), "'it'\"'\"'s'");
        assert_eq!(sh_quote("a b;c"), "'a b;c'");
    }

    #[test]
    fn truncate_output_respects_cap_and_boundary() {
        let s = "x".repeat(100);
        let out = truncate_output(&s, 8192);
        assert_eq!(out.len(), 100, "未超限原样返回");
        let long = "é".repeat(5000); // 多字节
        let out = truncate_output(&long, 8192);
        assert!(out.len() < 11000);
        assert!(out.contains("截断"));
        // 截在 UTF-8 边界上（不会 panic 即通过）
        let _ = truncate_output("密码密码密码", 7);
    }

    #[test]
    fn remote_parent_dir_variants() {
        assert_eq!(
            remote_parent_dir("/usr/local/bin/agent").as_deref(),
            Some("/usr/local/bin")
        );
        assert_eq!(
            remote_parent_dir("/agent").as_deref(),
            None,
            "根下文件无需 mkdir"
        );
        assert_eq!(
            remote_parent_dir("agent").as_deref(),
            None,
            "裸文件名无需 mkdir"
        );
        assert_eq!(remote_parent_dir("rel/dir/f").as_deref(), Some("rel/dir"));
    }

    // ============ 路由声明测试 ============

    #[tokio::test]
    async fn routes_declares_thirty_one_endpoints() {
        let h = ProvisioningRouteHandler::new();
        let routes = h.routes().await;
        assert_eq!(routes.len(), 31, "应有 31 条路由: {routes:?}");
        // 新增端点在列
        assert!(routes.iter().any(|r| r.method == HttpMethod::Post
            && r.path == "/api/v1/provisioning/iso/tasks/:id/build"));
        assert!(routes
            .iter()
            .any(|r| r.method == HttpMethod::Get && r.path == "/api/v1/provisioning/ssh/deploys"));
        // 网络装机 P0：bootstrap/菜单/种子公开，scan/delete 收 admin
        assert!(routes.iter().any(
            |r| r.method == HttpMethod::Get && r.path == "/api/v1/provisioning/bootstrap.ipxe"
        ));
        assert!(routes
            .iter()
            .any(|r| r.method == HttpMethod::Get && r.path == "/api/v1/provisioning/ipxe/menu"));
        assert!(routes
            .iter()
            .any(|r| r.method == HttpMethod::Get
                && r.path == "/api/v1/provisioning/seed/:id/user-data"));
        assert!(routes
            .iter()
            .any(|r| r.method == HttpMethod::Get && r.path == "/api/v1/provisioning/isos"));
        assert!(routes.iter().any(|r| r.method == HttpMethod::Post
            && r.path == "/api/v1/provisioning/isos/scan"
            && r.requires_auth));
        assert!(routes.iter().any(|r| r.method == HttpMethod::Delete
            && r.path == "/api/v1/provisioning/isos/:file"
            && r.requires_auth));
    }

    #[tokio::test]
    async fn install_bootstrap_routes_declared_with_auth_policy() {
        let h = ProvisioningRouteHandler::new();
        let routes = h.routes().await;
        // install.sh：公开（NAT 新机无 token 可达）
        let install = routes
            .iter()
            .find(|r| r.path == "/api/v1/provisioning/install.sh")
            .unwrap_or_else(|| panic!("install.sh 路由必须声明: {routes:?}"));
        assert_eq!(install.method, HttpMethod::Get);
        assert!(!install.requires_auth, "install.sh 必须公开");
        // prepare：admin 写
        let prepare = routes
            .iter()
            .find(|r| r.path == "/api/v1/provisioning/prepare-distributable")
            .expect("prepare-distributable 路由必须声明");
        assert_eq!(prepare.method, HttpMethod::Post);
        assert!(prepare.requires_auth);
        assert_eq!(prepare.required_roles, vec!["admin".to_string()]);
        // dist 白名单下载：公开读
        let dist = routes
            .iter()
            .find(|r| r.path == "/api/v1/provisioning/dist/:artifact")
            .expect("dist/:artifact 路由必须声明");
        assert_eq!(dist.method, HttpMethod::Get);
        assert!(!dist.requires_auth, "dist 下载公开（装好前无 token）");
    }

    #[tokio::test]
    async fn routes_all_belong_to_provisioning() {
        let h = ProvisioningRouteHandler::new();
        let routes = h.routes().await;
        assert!(
            routes.iter().all(|r| r.handler_component == "provisioning"),
            "全部归属 provisioning 组件"
        );
        // 写操作都要求 admin
        for r in &routes {
            if r.method == HttpMethod::Post || r.method == HttpMethod::Delete {
                assert!(r.requires_auth, "写操作需 auth: {r:?}");
                assert_eq!(r.required_roles, vec!["admin".to_string()]);
            }
        }
        // GET 默认公开；部署读端点（含远程路径/命令输出）收紧为 admin
        for r in &routes {
            if r.method == HttpMethod::Get {
                let sensitive = r.path == "/api/v1/provisioning/ssh/deploys"
                    || r.path == "/api/v1/provisioning/ssh/deploy/:id";
                assert_eq!(r.requires_auth, sensitive, "GET 鉴权策略: {r:?}");
            }
        }
    }

    // ============ stats 聚合 ============

    #[tokio::test]
    async fn stats_aggregates_counts() {
        let h = ProvisioningRouteHandler::new();
        let resp = h
            .handle(get_req("/api/v1/provisioning/stats"))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body["pxe_running"], false);
        assert_eq!(resp.body["iso_tasks_total"], 1);
        assert_eq!(resp.body["iso_completed"], 1);
        assert_eq!(resp.body["iso_failed"], 0);
        assert_eq!(resp.body["ssh_targets_total"], 1);
        assert_eq!(
            resp.body["ssh_reachable"], 0,
            "初始 unknown，不计 reachable"
        );
        assert_eq!(resp.body["deploys_total"], 0);
    }

    // ============ 一键安装引导（install.sh / prepare-distributable / dist）============

    // —— GET install.sh：Host 头推导安装源 + bootstrap/脚本形状断言 ——
    #[tokio::test]
    async fn install_sh_generated_with_host_header_and_bootstrap() {
        let h = ProvisioningRouteHandler::with_empty();
        let resp = h
            .handle(get_req_headers(
                "/api/v1/provisioning/install.sh",
                serde_json::json!({"host": "203.0.113.2:8558"}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status, 200, "body: {resp:?}");
        // 直传契约：content-type 为 shell 脚本文本，body 是未加引号的字符串
        assert_eq!(
            resp.headers["content-type"], "text/x-shellscript; charset=utf-8",
            "headers: {:?}",
            resp.headers
        );
        let script = resp.body.as_str().expect("body 应为脚本文本");
        assert!(script.starts_with("#!/usr/bin/env bash"), "开头: {script}");
        // 安装源来自 Host 头（任一节点可当安装源）
        assert!(
            script.contains("http://203.0.113.2:8558"),
            "应嵌入 Host 头推导的安装源: {script}"
        );
        // bootstrap：两个固定公网入口都在缺省列表（第一交互对象为公网入口）
        assert!(
            script.contains(PUBLIC_ENTRY_ALIYUN) && script.contains(PUBLIC_ENTRY_ANCHOR),
            "bootstrap 缺省应含公网入口: {script}"
        );
        let bootstrap_line = format!("NEXOS_BOOTSTRAP_DEFAULT='{}'", default_bootstrap_list());
        assert!(
            script.contains(&bootstrap_line),
            "应整行烘焙 bootstrap 缺省值 {bootstrap_line}: {script}"
        );
        // 无残留占位符
        assert!(!script.contains("@@"), "占位符必须全部替换: {script}");
        // systemd unit 的 P2P env 全套在位
        for env_line in [
            "Environment=NEXOS_P2P_ENABLE=1",
            "Environment=NEXOS_P2P_NAME=",
            "Environment=NEXOS_P2P_BOOTSTRAP=$BOOTSTRAP_LIST",
            "Environment=NEXOS_P2P_LISTEN=:7070",
            "Environment=NEXOS_GIT_ADVERTISE_HOST=$EGRESS_IP",
            "Environment=NEXOS_UPDATE_REPO_URL=$SRC/git/nexos.git",
        ] {
            assert!(script.contains(env_line), "unit 应含 {env_line}: {script}");
        }
    }

    // —— POST prepare-distributable：幂等（重跑覆盖旧分发件，sha 对拍一致）——
    #[test]
    fn prepare_distributable_idempotent_and_overwrites() {
        let dir = temp_dir("prepare");
        std::fs::create_dir_all(&dir).unwrap();
        // 独占窗口内改写分发目录 env（与其它 env 注入测试互斥）
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var("NEXOS_DISTRIBUTABLE_DIR").ok();
        std::env::set_var("NEXOS_DISTRIBUTABLE_DIR", &dir);
        fake_rt().block_on(async {
            let h = ProvisioningRouteHandler::with_empty();
            // 预置一份"旧版本"脏数据——prepare 必须原子覆盖之
            let target = dir.join(DISTRIBUTABLE_BIN_NAME);
            std::fs::write(&target, b"stale-v0-binary").unwrap();

            let resp = h
                .handle(post_req(
                    "/api/v1/provisioning/prepare-distributable",
                    serde_json::Value::Null,
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 200, "首次 prepare 应成功: {resp:?}");
            // with_empty 未注入 update registry → 不自动登记（兼容历史行为；
            // 注入路径的登记断言见 prepare_distributable_registers_update_artifact）
            assert!(
                resp.body["update_artifact"].is_null(),
                "未注入 registry 时不应回传登记结果: {resp:?}"
            );
            let sha1 = resp.body["sha256"].as_str().unwrap().to_string();
            let size1 = resp.body["size_bytes"].as_u64().unwrap();
            assert_eq!(sha1.len(), 64, "sha256 应为 hex 摘要");
            assert!(
                size1 > 16,
                "当前可执行文件不可能只有十几字节（旧脏数据未覆盖？size={size1}）"
            );
            assert_eq!(
                resp.body["download_path"],
                "/api/v1/provisioning/dist/os-api"
            );
            assert_eq!(
                resp.body["path"].as_str().unwrap(),
                target.display().to_string()
            );
            // 落盘完整性：磁盘内容哈希 == 响应 sha256
            let on_disk = std::fs::read(&target).unwrap();
            let mut hh = Sha256::new();
            hh.update(&on_disk);
            assert_eq!(format!("{:x}", hh.finalize()), sha1);
            assert_ne!(
                &on_disk[..],
                b"stale-v0-binary",
                "旧脏文件必须被真实二进制覆盖"
            );

            // 幂等：重跑仍成功且结果一致（同 size 同 sha）
            let resp2 = h
                .handle(post_req(
                    "/api/v1/provisioning/prepare-distributable",
                    serde_json::Value::Null,
                ))
                .await
                .unwrap();
            assert_eq!(resp2.status, 200, "重复 prepare 应幂等成功: {resp2:?}");
            assert_eq!(resp2.body["sha256"], resp.body["sha256"]);
            assert_eq!(resp2.body["size_bytes"], resp.body["size_bytes"]);
        });
        match old {
            Some(v) => std::env::set_var("NEXOS_DISTRIBUTABLE_DIR", v),
            None => std::env::remove_var("NEXOS_DISTRIBUTABLE_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // —— POST prepare-distributable：自动登记同版本更新工件（页内 apply 通道
    //    2026-09-03 发版双通道接线：三节点各跑一次 prepare 即同时喂饱 dist
    //    下载与页内 apply——不再需要手动 POST /update/artifact）——
    #[test]
    fn prepare_distributable_registers_update_artifact() {
        let dir = temp_dir("prepare-reg");
        std::fs::create_dir_all(&dir).unwrap();
        // 独占窗口内改写分发目录 env（与其它 env 注入测试互斥）
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var("NEXOS_DISTRIBUTABLE_DIR").ok();
        std::env::set_var("NEXOS_DISTRIBUTABLE_DIR", &dir);
        // 共享 update 实例：低当前版本（0.0.1 < CARGO_PKG_VERSION）使 apply
        // 前置成立；状态 JSON 落同目录（重开读回断言用）；with_config 构造
        // → self_restart 恒 false（测试红线：绝不 systemctl）。
        let state = dir.join("update-state.json");
        let update = Arc::new(UpdateRouteHandler::with_config(
            Some(state.to_string_lossy().into_owned()),
            "/nonexistent/nexos.git",
            "0.0.1",
        ));
        fake_rt().block_on(async {
            let h = ProvisioningRouteHandler::with_empty().with_update_registry(update.clone());
            let resp = h
                .handle(post_req(
                    "/api/v1/provisioning/prepare-distributable",
                    serde_json::Value::Null,
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 200, "prepare 应成功: {resp:?}");
            // 响应回传登记结果：version=运行二进制版本，path/sha 与暂存件同源。
            let art = &resp.body["update_artifact"];
            assert_eq!(
                art["version"],
                env!("CARGO_PKG_VERSION"),
                "登记版本 = 运行二进制 CARGO_PKG_VERSION: {resp:?}"
            );
            let target = dir.join(DISTRIBUTABLE_BIN_NAME);
            assert_eq!(
                art["path"].as_str().unwrap(),
                target.display().to_string(),
                "登记 path = 分发产物路径"
            );
            assert_eq!(
                art["sha256"], resp.body["sha256"],
                "登记 sha 与暂存 sha 同源"
            );
            // 共享实例侧立即可见（同一 Mutex 态，非复制）：GET /update/artifacts。
            let arts = update
                .handle(get_req("/api/v1/update/artifacts"))
                .await
                .unwrap();
            let list = arts.body.as_array().unwrap();
            assert_eq!(list.len(), 1, "prepare 后工件表即有同版本条目: {list:?}");
            assert_eq!(list[0]["version"], env!("CARGO_PKG_VERSION"));
            assert_eq!(
                list[0]["path"].as_str().unwrap(),
                target.display().to_string()
            );
            // 页内 apply 通道打通：apply 直接建任务（不再报"尚未登记更新工件"）。
            // 只建任务不轮询——推进到 writing 会动 exec 路径（测试红线）。
            let apply = update
                .handle(post_req(
                    "/api/v1/update/apply",
                    serde_json::json!({"version": env!("CARGO_PKG_VERSION")}),
                ))
                .await
                .unwrap();
            assert_eq!(
                apply.status, 201,
                "prepare 后页内 apply 应可直接建任务: {apply:?}"
            );
            assert_eq!(apply.body["status"], "pending");
            assert!(
                apply.body["artifact_path"]
                    .as_str()
                    .is_some_and(|p| p.ends_with(DISTRIBUTABLE_BIN_NAME)),
                "任务快照的工件路径应为分发产物: {apply:?}"
            );
            // 幂等：重复 prepare → 工件表仍 1 条（同 version 覆盖，不增条目）。
            let resp2 = h
                .handle(post_req(
                    "/api/v1/provisioning/prepare-distributable",
                    serde_json::Value::Null,
                ))
                .await
                .unwrap();
            assert_eq!(resp2.status, 200);
            let arts2 = update
                .handle(get_req("/api/v1/update/artifacts"))
                .await
                .unwrap();
            assert_eq!(
                arts2.body.as_array().unwrap().len(),
                1,
                "重复 prepare 幂等（同 version 覆盖不增条目）"
            );
            // 未注入 registry（历史构造/单测缺省）的兼容断言见
            // prepare_distributable_idempotent_and_overwrites（with_empty 构造）。
        });
        // 重启读回：登记随 update-state.json 持久化（update 组件重建后可见）。
        let update2 = UpdateRouteHandler::with_config(
            Some(state.to_string_lossy().into_owned()),
            "/nonexistent/nexos.git",
            "0.0.1",
        );
        fake_rt().block_on(async {
            let arts = update2
                .handle(get_req("/api/v1/update/artifacts"))
                .await
                .unwrap();
            assert_eq!(
                arts.body.as_array().unwrap().len(),
                1,
                "prepare 的登记应随 update-state.json 持久化"
            );
        });
        match old {
            Some(v) => std::env::set_var("NEXOS_DISTRIBUTABLE_DIR", v),
            None => std::env::remove_var("NEXOS_DISTRIBUTABLE_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // —— stage_artifact：sha256 已知向量 + 无 .part 残留 ——
    #[test]
    fn stage_artifact_sha256_known_vector() {
        let dir = temp_dir("sha-vec");
        let exe = dir.join("abc-exe");
        std::fs::write(&exe, b"abc").unwrap();
        let out = dir.join("dist").join("latest-os-api.bin");
        let prepared = stage_artifact(&exe, &out).expect("stage 应成功");
        assert_eq!(
            prepared.sha256, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "SHA256(\"abc\") 标准向量"
        );
        assert_eq!(prepared.size_bytes, 3);
        assert_eq!(std::fs::read(&out).unwrap(), b"abc", "内容应原样复制");
        // 原子暂存无 .part 残留
        assert!(
            !dir.join("dist").join("latest-os-api.tmp.part").exists(),
            "rename 后临时文件不应存在"
        );
        // 目标目录不存在时自动创建（fresh dist 目录即证）
        let _ = std::fs::remove_dir_all(&dir);
    }

    // —— render_install_script：token 全替换 + 单引号净化 ——
    #[test]
    fn install_sh_render_replaces_tokens_and_sanitizes_quotes() {
        let out = render_install_script(
            "http://203.0.113.9:8558",
            "203.0.113.2:7070,198.51.100.114:7070",
            "c0ffee00",
            "deadbeef",
        );
        assert!(out.starts_with("#!/usr/bin/env bash"));
        assert!(out.contains("NEXOS_SOURCE_DEFAULT='http://203.0.113.9:8558'"));
        assert!(out.contains("NEXOS_BOOTSTRAP_DEFAULT='203.0.113.2:7070,198.51.100.114:7070'"));
        assert!(out.contains("NEXOS_SHA256_EXPECTED='c0ffee00'"));
        assert!(out.contains("NEXOS_SHA256_EXPECTED_AARCH64='deadbeef'"));
        assert!(!out.contains("@@"), "占位符必须全部替换");
        // 注入面最小处理：单引号被剥除（值只进 bash 单引号字面量）
        let evil = render_install_script("http://o'brien:8558',x='y", "a:1", "", "");
        assert!(
            evil.contains("NEXOS_SOURCE_DEFAULT='http://obrien:8558,x=y'"),
            "单引号应被剥除: {}",
            &evil[..evil.len().min(400)]
        );
        // 空 sha → 脚本仍完整生成（跳过对拍）
        assert!(
            render_install_script("http://s:1", "b:1", "", "").contains("NEXOS_SHA256_EXPECTED=''")
        );
        // 常量与模板防漂移：缺省 token 文案（测试期兜底值，文档提示装完更换）
        assert!(
            INSTALL_SCRIPT_TEMPLATE.contains("{TOKEN:-change-me-admin-token}"),
            "模板默认 token 文案漂移（应为 change-me-admin-token 兜底）"
        );
    }

    // —— 架构自动分流：uname -m 三分支（x86_64 → os-api / aarch64 →
    //    os-api-aarch64 / 其他架构 die 列出可用架构）+ 仓库副本同源防漂移 ——
    #[test]
    fn install_sh_arch_dispatch_branches() {
        let out = render_install_script("http://s:8558", "b:1", "sha-x86", "sha-arm");
        // 探测动作在位
        assert!(out.contains("uname -m"), "脚本必须用 uname -m 探测架构");
        // x86_64 分支 → dist/os-api；aarch64 分支 → dist/os-api-aarch64
        assert!(out.contains("ARTIFACT='os-api'"), "x86_64 分支工件: {out}");
        assert!(
            out.contains("ARTIFACT='os-api-aarch64'"),
            "aarch64 分支工件（DGX Spark）: {out}"
        );
        assert!(
            out.contains("$SRC/api/v1/provisioning/dist/$ARTIFACT"),
            "下载 URL 应按分流出的工件名拼接"
        );
        // 其他架构 → die，提示不支持并列出可用架构
        let die_line = out
            .lines()
            .find(|l| l.contains("不支持的架构"))
            .expect("必须存在其他架构 die 分支");
        assert!(
            die_line.contains("x86_64") && die_line.contains("aarch64"),
            "die 提示应列出可用架构: {die_line}"
        );
        // 分流后期望 sha 随架构切换（双架构对拍各自独立烘焙）
        assert!(out.contains("EXPECTED_SHA=\"$NEXOS_SHA256_EXPECTED\""));
        assert!(out.contains("EXPECTED_SHA=\"$NEXOS_SHA256_EXPECTED_AARCH64\""));
        // 仓库独立副本同源：三分支一个不少（防手改漂移）
        let repo_copy = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../scripts/install-nexos.sh"),
        )
        .expect("仓库副本 scripts/install-nexos.sh 必须存在");
        for frag in [
            "uname -m",
            "ARTIFACT='os-api'",
            "ARTIFACT='os-api-aarch64'",
            "不支持的架构",
            "$SRC/api/v1/provisioning/dist/$ARTIFACT",
        ] {
            assert!(repo_copy.contains(frag), "仓库副本缺架构分流片段 `{frag}`");
        }
    }

    // —— bash -n 语法门：动态渲染版 + 仓库独立副本双双通过 ——
    #[test]
    fn install_sh_passes_bash_n_both_variants() {
        use std::process::Command;
        let dir = temp_dir("bash-n");
        let rendered = dir.join("install-nexos.sh");
        std::fs::write(
            &rendered,
            render_install_script("http://203.0.113.9:8558", "203.0.113.2:7070", "", ""),
        )
        .unwrap();
        let repo_copy = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../scripts/install-nexos.sh")
            .canonicalize()
            .expect("仓库独立副本 scripts/install-nexos.sh 必须随 commit 提交");
        for script in [rendered, repo_copy] {
            let out = Command::new("bash")
                .arg("-n")
                .arg(&script)
                .output()
                .expect("bash 不在 PATH（Linux CI/开发机必有）");
            assert!(
                out.status.success(),
                "bash -n {}: {}",
                script.display(),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // —— 版本感知升级：脚本片段形状 + 仓库副本同源 ——
    #[test]
    fn install_sh_version_aware_fragments_and_repo_sync() {
        let out = render_install_script("http://s:8558", "b:1", "sha-x86", "sha-arm");
        // 升级判定三要素：本地 sha 对拍烘焙期望值 → 跳过 / 自动升级 / 无期望值保持
        assert!(
            out.contains("sha256sum \"$BIN_PATH\""),
            "必须对本地二进制算 sha256: {out}"
        );
        assert!(
            out.contains("\"$LOCAL_SHA\" == \"$EXPECTED_SHA\""),
            "sha 一致分支必须跳过下载: {out}"
        );
        assert!(
            out.contains("已是源端最新构建"),
            "sha 一致应提示已最新并跳过: {out}"
        );
        assert!(
            out.contains("自动升级") && out.contains("升级 os-api："),
            "sha 不一致应自动重下并提示升级 X→Y: {out}"
        );
        assert!(
            out.contains("构建已刷新"),
            "同版本重建（sha 变版本不变）应有独立提示: {out}"
        );
        // 版本提示：本地与临时文件各跑一次 --version（代码形态精确断言）
        assert!(
            out.contains("\"$BIN_PATH\" --version") && out.contains("\"$TMP_FILE\" --version"),
            "本地/新下载各一次 --version: {out}"
        );
        assert!(
            out.contains("|| true"),
            "--version 失败（旧/异构二进制）不得中断脚本: {out}"
        );
        // 仓库副本同源（防手改漂移）
        let repo_copy = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../scripts/install-nexos.sh"),
        )
        .expect("仓库副本 scripts/install-nexos.sh 必须存在");
        for frag in [
            "sha256sum \"$BIN_PATH\"",
            "已是源端最新构建",
            "升级 os-api：",
            "构建已刷新",
            "源端分发件未就绪",
        ] {
            assert!(repo_copy.contains(frag), "仓库副本缺版本感知片段 `{frag}`");
        }
    }

    /// 从渲染后的安装脚本截取「—— 3) 下载 os-api 二进制」步（到「—— 4)」前）。
    fn extract_install_step3(script: &str) -> String {
        let start = script
            .lines()
            .position(|l| l.contains("—— 3) 下载 os-api"))
            .expect("step-3 标记行必须存在");
        let end = script
            .lines()
            .position(|l| l.contains("—— 4) systemd"))
            .expect("step-4 标记行必须存在");
        assert!(start < end, "step-3 必须在 step-4 之前");
        script
            .lines()
            .skip(start)
            .take(end - start)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 文件 sha256（hex）。
    fn file_sha256(path: &std::path::Path) -> String {
        use sha2::Digest;
        let data = std::fs::read(path).unwrap();
        format!("{:x}", sha2::Sha256::digest(&data))
    }

    /// 在 bash 沙箱里跑 step-3 片段（stub 掉 curl/log/warn/die；sha256sum/
    /// head/awk/chmod/mv 用真实 coreutils）。返回 (退出码, 合并输出)。
    fn run_install_step3(
        step3: &str,
        bin_path: &std::path::Path,
        payload: &std::path::Path,
        expected_sha: &str,
        force: u8,
    ) -> (i32, String) {
        use std::process::Command;
        let install_dir = bin_path.parent().unwrap().display().to_string();
        let bin = bin_path.display().to_string();
        let payload = payload.display().to_string();
        let harness = format!(
            r#"set -euo pipefail
SRC='http://127.0.0.1:1'
ARTIFACT='os-api'
MACHINE_ARCH='x86_64'
INSTALL_DIR='{install_dir}'
BIN_PATH='{bin}'
EXPECTED_SHA='{expected_sha}'
FORCE={force}
CURL_CALLS=0
PAYLOAD='{payload}'
curl() {{
  CURL_CALLS=$((CURL_CALLS + 1))
  local out=''
  while [[ $# -gt 0 ]]; do
    case "$1" in
      -o) out="$2"; shift 2 ;;
      *) shift ;;
    esac
  done
  cp "$PAYLOAD" "$out"
}}
log()  {{ printf 'LOG %s\n' "$*"; }}
warn() {{ printf 'WARN %s\n' "$*" >&2; }}
die()  {{ printf 'DIE %s\n' "$*" >&2; exit 1; }}
{step3}
printf 'CURL_CALLS=%d\n' "$CURL_CALLS"
"#
        );
        let out = Command::new("/bin/bash")
            .arg("-c")
            .arg(&harness)
            // 并行 PATH 注入隔离：其他测试会把进程 PATH 换成假工具链目录
            //（假 sha256sum），子进程显式给定干净 PATH + C locale
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .env("LC_ALL", "C")
            .output()
            .expect("/bin/bash 必在（Ubuntu 22.04/24.04 目标环境）");
        let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
        combined.push_str(&String::from_utf8_lossy(&out.stderr));
        (out.status.code().unwrap_or(-1), combined)
    }

    /// 选两个版本串不同、复制后 `--version` 仍可跑的系统 ELF 当"本地/新下载"
    /// 二进制（/usr/bin/env 是 uutils 套件——按 argv[0] 分发程序，复制改名后
    /// 不可用，故选 bash / python3 这类不认自身文件名的）。
    fn elf_pair_for_upgrade_test() -> (std::path::PathBuf, std::path::PathBuf) {
        let a = std::path::PathBuf::from("/bin/bash");
        let b = std::path::PathBuf::from("/usr/bin/python3");
        assert!(
            a.is_file() && b.is_file(),
            "测试依赖 /bin/bash 与 /usr/bin/python3"
        );
        (a, b)
    }

    // —— 版本感知升级行为自测：真跑 step-3 片段的四条分支 ——
    // （bash -n 之外的行为门：跳过/升级/强制/源端未就绪，全走真实 bash + coreutils）
    #[test]
    fn install_sh_step3_behavior_same_sha_skips() {
        let dir = temp_dir("step3-same");
        let (old, _new) = elf_pair_for_upgrade_test();
        let bin = dir.join("os-api");
        std::fs::copy(&old, &bin).unwrap();
        let (code, out) = run_install_step3(
            &extract_install_step3(&render_install_script("http://s:1", "b:1", "", "")),
            &bin,
            &old,
            &file_sha256(&old),
            0,
        );
        assert_eq!(code, 0, "同 sha 分支应零退出: {out}");
        assert!(out.contains("已是源端最新构建"), "{out}");
        assert!(out.contains("CURL_CALLS=0"), "同 sha 不应下载: {out}");
        assert_eq!(file_sha256(&bin), file_sha256(&old), "二进制不应被改动");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_sh_step3_behavior_new_sha_upgrades_with_version_message() {
        let dir = temp_dir("step3-up");
        let (old, new) = elf_pair_for_upgrade_test();
        assert_ne!(file_sha256(&old), file_sha256(&new));
        let bin = dir.join("os-api");
        std::fs::copy(&old, &bin).unwrap();
        let (code, out) = run_install_step3(
            &extract_install_step3(&render_install_script("http://s:1", "b:1", "", "")),
            &bin,
            &new,
            &file_sha256(&new),
            0,
        );
        assert_eq!(code, 0, "升级分支应零退出: {out}");
        assert!(out.contains("自动升级"), "{out}");
        assert!(out.contains("升级 os-api："), "应提示升级 X→Y: {out}");
        assert!(out.contains("CURL_CALLS=1"), "{out}");
        assert_eq!(
            file_sha256(&bin),
            file_sha256(&new),
            "二进制应被替换为新分发件"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_sh_step3_behavior_force_redownloads_same_content() {
        let dir = temp_dir("step3-force");
        let (old, _new) = elf_pair_for_upgrade_test();
        let bin = dir.join("os-api");
        std::fs::copy(&old, &bin).unwrap();
        let (code, out) = run_install_step3(
            &extract_install_step3(&render_install_script("http://s:1", "b:1", "", "")),
            &bin,
            &old,
            &file_sha256(&old),
            1,
        );
        assert_eq!(code, 0, "--force 应零退出: {out}");
        assert!(
            out.contains("CURL_CALLS=1"),
            "--force 必须无条件重下: {out}"
        );
        assert!(
            out.contains("构建已刷新"),
            "同版本强制重下应提示构建刷新: {out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn install_sh_step3_behavior_missing_expected_sha_keeps_binary() {
        let dir = temp_dir("step3-nosha");
        let (old, _new) = elf_pair_for_upgrade_test();
        let bin = dir.join("os-api");
        std::fs::copy(&old, &bin).unwrap();
        let (code, out) = run_install_step3(
            &extract_install_step3(&render_install_script("http://s:1", "b:1", "", "")),
            &bin,
            &old,
            "",
            0,
        );
        assert_eq!(code, 0, "无期望 sha 应保持现状零退出: {out}");
        assert!(out.contains("WARN"), "应告警源端分发件未就绪: {out}");
        assert!(out.contains("CURL_CALLS=0"), "无期望 sha 不应下载: {out}");
        assert_eq!(file_sha256(&bin), file_sha256(&old));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 从渲染后的安装脚本截取「—— 4) systemd 服务」步（到「—— 5) 健康确认」前）。
    fn extract_install_step4(script: &str) -> String {
        let start = script
            .lines()
            .position(|l| l.contains("—— 4) systemd"))
            .expect("step-4 标记行必须存在");
        let end = script
            .lines()
            .position(|l| l.contains("—— 5) 健康确认"))
            .expect("step-5 标记行必须存在");
        assert!(start < end, "step-4 必须在 step-5 之前");
        script
            .lines()
            .skip(start)
            .take(end - start)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 在 bash 沙箱里跑 step-4 片段（stub 掉 hostname/systemctl/log/die；
    /// heredoc 生成的 unit 文件是真实文件 I/O）。返回 (退出码, unit 文件内容,
    /// 合并输出)。
    fn run_install_step4(step4: &str, unit_path: &str, src: &str) -> (i32, String, String) {
        use std::process::Command;
        let harness = format!(
            r#"set -euo pipefail
SRC='{src}'
PORT='8558'
INSTALL_DIR='/tmp/nexos-step4-install'
BIN_PATH='/tmp/nexos-step4-install/os-api'
UNIT_PATH='{unit_path}'
SERVICE_NAME='nexos-os-api'
NEED_DOWNLOAD=0
EGRESS_IP='192.0.2.10'
NEXOS_BOOTSTRAP_DEFAULT='b:7070'
NAME=''
BOOTSTRAP=''
TOKEN=''
hostname() {{ printf 'spark-host\n'; }}
systemctl() {{ return 0; }}
log()  {{ printf 'LOG %s\n' "$*"; }}
warn() {{ printf 'WARN %s\n' "$*" >&2; }}
die()  {{ printf 'DIE %s\n' "$*" >&2; exit 1; }}
{step4}
printf '%s\n' '---UNIT---'
cat '{unit_path}'
"#
        );
        let out = Command::new("/bin/bash")
            .arg("-c")
            .arg(&harness)
            // 并行 PATH 注入隔离（同 run_install_step3）
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .env("LC_ALL", "C")
            .output()
            .expect("/bin/bash 必在（Ubuntu 22.04/24.04 目标环境）");
        let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
        combined.push_str(&String::from_utf8_lossy(&out.stderr));
        let unit = combined
            .split("---UNIT---\n")
            .nth(1)
            .unwrap_or_default()
            .to_string();
        (out.status.code().unwrap_or(-1), unit, combined)
    }

    // —— 更新源引导 env 行：形状断言 + 仓库副本同源防漂移 ——
    #[test]
    fn install_sh_update_repo_url_env_line_shape() {
        let out = render_install_script("http://203.0.113.2:8558", "b:1", "", "");
        // unit 模板注入 NEXOS_UPDATE_REPO_URL=$SRC/git/nexos.git（$SRC 运行时展开；
        // 计数用 Environment= 前缀——step-4 说明注释亦提及该 env 名，不算注入行）
        assert!(
            out.contains("Environment=NEXOS_UPDATE_REPO_URL=$SRC/git/nexos.git"),
            "unit 应注入更新源 URL env 行（$SRC 形态）: {out}"
        );
        assert_eq!(
            out.matches("Environment=NEXOS_UPDATE_REPO_URL").count(),
            1,
            "env 注入行应恰好一条"
        );
        // 仓库副本同源（防手改漂移）
        let repo_copy = std::fs::read_to_string(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../scripts/install-nexos.sh"),
        )
        .expect("仓库副本 scripts/install-nexos.sh 必须存在");
        assert!(
            repo_copy.contains("Environment=NEXOS_UPDATE_REPO_URL=$SRC/git/nexos.git"),
            "仓库副本缺更新源 env 行"
        );
    }

    // —— 更新源引导 env 行行为门：真跑 step-4 片段，断言写入/幂等/换源跟随 ——
    #[test]
    fn install_sh_step4_update_repo_url_written_idempotent_and_follows_source() {
        let dir = temp_dir("step4-updrepo");
        std::fs::create_dir_all(&dir).unwrap();
        let unit = dir.join("nexos-os-api.service");
        let unit_str = unit.to_string_lossy().into_owned();
        let step4 = extract_install_step4(&render_install_script("http://s:8558", "b:1", "", ""));
        // 首跑：安装源 aliyun → env 行展开写入 unit 文件
        let (code, unit1, out) = run_install_step4(&step4, &unit_str, "http://203.0.113.2:8558");
        assert_eq!(code, 0, "step-4 应零退出: {out}");
        let want = "Environment=NEXOS_UPDATE_REPO_URL=http://203.0.113.2:8558/git/nexos.git";
        assert!(unit1.contains(want), "unit 应含 {want}: {unit1}");
        assert_eq!(
            unit1.matches("NEXOS_UPDATE_REPO_URL").count(),
            1,
            "env 行应恰好一条（幂等写入）: {unit1}"
        );
        // 重跑（同源）：unit 整文件重写 → 行不变且仍唯一（幂等）
        let (code2, unit2, out2) = run_install_step4(&step4, &unit_str, "http://203.0.113.2:8558");
        assert_eq!(code2, 0, "重跑应零退出: {out2}");
        assert!(unit2.contains(want), "同源重跑 env 行应保持: {unit2}");
        assert_eq!(unit2.matches("NEXOS_UPDATE_REPO_URL").count(), 1);
        // 换源重跑（--source 指向另一节点）：env 行随 $SRC 更新
        let (code3, unit3, out3) = run_install_step4(&step4, &unit_str, "http://198.51.100.114:8558");
        assert_eq!(code3, 0, "换源重跑应零退出: {out3}");
        let want_new = "Environment=NEXOS_UPDATE_REPO_URL=http://198.51.100.114:8558/git/nexos.git";
        assert!(unit3.contains(want_new), "换源后 env 行应跟随: {unit3}");
        assert!(!unit3.contains(want), "旧源 URL 不应残留: {unit3}");
        assert_eq!(unit3.matches("NEXOS_UPDATE_REPO_URL").count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // —— 生成脚本 --help 分支可真跑（参数解析段在 root 检查前）——
    #[test]
    fn install_sh_help_branch_runs() {
        use std::process::Command;
        let dir = temp_dir("help-run");
        let rendered = dir.join("install-nexos.sh");
        std::fs::write(
            &rendered,
            render_install_script("http://203.0.113.9:8558", "203.0.113.2:7070", "", ""),
        )
        .unwrap();
        let out = Command::new("/bin/bash")
            .arg(&rendered)
            .arg("--help")
            // 同 run_install_step3：并行 PATH 注入隔离（--help 分支虽不调外部
            // 命令，程序解析也走绝对路径避免歧义）
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .env("LC_ALL", "C")
            .output()
            .expect("/bin/bash 必在");
        assert!(
            out.status.success(),
            "--help 应零退出: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("用法"), "应打印用法: {stdout}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // —— GET dist/:artifact：白名单防穿越 + base64→原始字节 + 未就绪指引 ——
    #[test]
    fn dist_artifact_traversal_guard_whitelist() {
        use base64::Engine as _;
        let dir = temp_dir("dist-guard");
        std::fs::create_dir_all(&dir).unwrap();
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var("NEXOS_DISTRIBUTABLE_DIR").ok();
        std::env::set_var("NEXOS_DISTRIBUTABLE_DIR", &dir);
        fake_rt().block_on(async {
            let h = ProvisioningRouteHandler::with_empty();

            // （a）工件未就绪 → 404 且给 prepare 指引
            let missing = h
                .handle(get_req("/api/v1/provisioning/dist/os-api"))
                .await
                .unwrap();
            assert_eq!(missing.status, 404, "body: {missing:?}");
            assert!(
                missing.body["error"]
                    .as_str()
                    .unwrap()
                    .contains("prepare-distributable"),
                "404 应指路 prepare: {:?}",
                missing.body
            );

            // （b）合法工件 → octet-stream + base64 body 解码回原始字节 + sha 头
            let payload = b"\x7fELF-fake-os-api-payload";
            std::fs::write(dir.join(DISTRIBUTABLE_BIN_NAME), payload).unwrap();
            let ok = h
                .handle(get_req("/api/v1/provisioning/dist/os-api"))
                .await
                .unwrap();
            assert_eq!(ok.status, 200, "body: {ok:?}");
            assert_eq!(ok.headers["content-type"], "application/octet-stream");
            let served = base64::engine::general_purpose::STANDARD
                .decode(ok.body.as_str().expect("body 应为 base64 字符串"))
                .unwrap();
            assert_eq!(served.as_slice(), payload, "base64 往返必须逐字节还原");
            let mut hh = Sha256::new();
            hh.update(payload);
            assert_eq!(
                ok.headers["x-nexos-sha256"],
                format!("{:x}", hh.finalize()),
                "sha 头供安装端对拍: {:?}",
                ok.headers
            );

            // （c）穿越/注入形态（单段可达本路由）：白名单外一律拒绝（不触碰 FS）
            for artifact in [
                "..",
                ".",
                "%2e%2e",
                "%2Fetc%2Fpasswd",
                "passwd",
                "os-api.bak",
            ] {
                let denied = h
                    .handle(get_req(&format!("/api/v1/provisioning/dist/{artifact}")))
                    .await
                    .unwrap();
                assert_eq!(
                    denied.status, 400,
                    "白名单外工件 `{artifact}` 必须拒绝: {denied:?}"
                );
                assert!(
                    denied.body["error"].as_str().unwrap().contains("未知工件"),
                    "`{artifact}` 错误应点名未知工件: {:?}",
                    denied.body
                );
            }

            // （d）多点路径注入超出段数 → 兜底 404（同样到不了文件系统）
            let deep = h
                .handle(get_req("/api/v1/provisioning/dist/../../etc/shadow"))
                .await
                .unwrap();
            assert_eq!(deep.status, 404, "越界段走兜底 404: {deep:?}");
        });
        match old {
            Some(v) => std::env::set_var("NEXOS_DISTRIBUTABLE_DIR", v),
            None => std::env::remove_var("NEXOS_DISTRIBUTABLE_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // —— dist 白名单扩展：os-api-aarch64 下载 / 未就绪 404 / 白名单外拒绝 ——
    #[test]
    fn dist_artifact_aarch64_whitelist_download() {
        use base64::Engine as _;
        let dir = temp_dir("dist-arm");
        std::fs::create_dir_all(dir.join("dist")).unwrap();
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var("NEXOS_DISTRIBUTABLE_DIR").ok();
        std::env::set_var("NEXOS_DISTRIBUTABLE_DIR", &dir);
        fake_rt().block_on(async {
            let h = ProvisioningRouteHandler::with_empty();

            // （a）aarch64 工件未就绪 → 404（脚本侧跳过对拍后落到这里的下载指引）
            let missing = h
                .handle(get_req("/api/v1/provisioning/dist/os-api-aarch64"))
                .await
                .unwrap();
            assert_eq!(missing.status, 404, "body: {missing:?}");
            assert!(
                missing.body["error"].as_str().unwrap().contains("aarch64"),
                "404 应点名 aarch64 工件: {:?}",
                missing.body
            );

            // （b）就绪（release.sh 刷新位 dist/os-api-aarch64-latest）→
            //     200 + base64 解码回原始字节 + sha 头 + 分流工件名 disposition
            let payload = b"\x7fELF-fake-aarch64-payload";
            std::fs::write(dir.join("dist").join("os-api-aarch64-latest"), payload).unwrap();
            let ok = h
                .handle(get_req("/api/v1/provisioning/dist/os-api-aarch64"))
                .await
                .unwrap();
            assert_eq!(ok.status, 200, "body: {ok:?}");
            assert_eq!(ok.headers["content-type"], "application/octet-stream");
            let served = base64::engine::general_purpose::STANDARD
                .decode(ok.body.as_str().expect("body 应为 base64 字符串"))
                .unwrap();
            assert_eq!(served.as_slice(), payload, "base64 往返必须逐字节还原");
            let mut hh = Sha256::new();
            hh.update(payload);
            assert_eq!(
                ok.headers["x-nexos-sha256"],
                format!("{:x}", hh.finalize()),
                "sha 头供 ARM 安装端对拍: {:?}",
                ok.headers
            );
            assert!(
                ok.headers["content-disposition"]
                    .as_str()
                    .unwrap()
                    .contains("os-api-aarch64"),
                "disposition 应按工件名: {:?}",
                ok.headers
            );

            // （c）白名单外（含形近名 / 他架构产物）一律拒绝
            for artifact in [
                "os-api-armhf",
                "os-api-x86_64-latest",
                "p2p-node-aarch64-latest",
            ] {
                let denied = h
                    .handle(get_req(&format!("/api/v1/provisioning/dist/{artifact}")))
                    .await
                    .unwrap();
                assert_eq!(
                    denied.status, 400,
                    "白名单外工件 `{artifact}` 必须拒绝: {denied:?}"
                );
            }
        });
        match old {
            Some(v) => std::env::set_var("NEXOS_DISTRIBUTABLE_DIR", v),
            None => std::env::remove_var("NEXOS_DISTRIBUTABLE_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // —— GET install.sh：双架构 sha256 各自烘焙（x86_64 / aarch64 独立对拍）——
    #[test]
    fn install_sh_endpoint_bakes_both_arch_sha256() {
        let dir = temp_dir("install-sha");
        std::fs::create_dir_all(dir.join("dist")).unwrap();
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var("NEXOS_DISTRIBUTABLE_DIR").ok();
        std::env::set_var("NEXOS_DISTRIBUTABLE_DIR", &dir);
        let x86_payload = b"\x7fELF-x86-bin-payload";
        let arm_payload = b"\x7fELF-aarch64-bin-payload";
        std::fs::write(dir.join(DISTRIBUTABLE_BIN_NAME), x86_payload).unwrap();
        std::fs::write(
            dir.join(artifact_fs_path(DISTRIBUTABLE_AARCH64_ARTIFACT)),
            arm_payload,
        )
        .unwrap();
        fake_rt().block_on(async {
            let h = ProvisioningRouteHandler::with_empty();
            let resp = h
                .handle(get_req_headers(
                    "/api/v1/provisioning/install.sh",
                    serde_json::json!({"host": "203.0.113.9:8558"}),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 200, "body: {resp:?}");
            let script = resp.body.as_str().expect("body 应为脚本文本");
            let mut hx = Sha256::new();
            hx.update(x86_payload);
            let x86_sha = format!("{:x}", hx.finalize());
            let mut ha = Sha256::new();
            ha.update(arm_payload);
            let arm_sha = format!("{:x}", ha.finalize());
            assert_ne!(x86_sha, arm_sha, "两份载荷摘要应互异（烘焙各归各）");
            assert!(
                script.contains(&format!("NEXOS_SHA256_EXPECTED='{x86_sha}'")),
                "x86_64 期望 sha 应整行烘焙: {script}"
            );
            assert!(
                script.contains(&format!("NEXOS_SHA256_EXPECTED_AARCH64='{arm_sha}'")),
                "aarch64 期望 sha 应整行烘焙: {script}"
            );
            assert!(!script.contains("@@"), "占位符必须全部替换");
        });
        match old {
            Some(v) => std::env::set_var("NEXOS_DISTRIBUTABLE_DIR", v),
            None => std::env::remove_var("NEXOS_DISTRIBUTABLE_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // —— artifact_fs_path：白名单名 → 固定文件映射（aarch64 落 release.sh 刷新位）——
    #[test]
    fn artifact_fs_path_maps_whitelist_names() {
        let dir = temp_dir("artifact-map");
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var("NEXOS_DISTRIBUTABLE_DIR").ok();
        std::env::set_var("NEXOS_DISTRIBUTABLE_DIR", &dir);
        assert_eq!(
            artifact_fs_path("os-api"),
            dir.join(DISTRIBUTABLE_BIN_NAME),
            "os-api 维持 x86_64 主件路径不变"
        );
        assert_eq!(
            artifact_fs_path(DISTRIBUTABLE_AARCH64_ARTIFACT),
            dir.join(DISTRIBUTABLE_AARCH64_REL_PATH),
            "os-api-aarch64 应映射到分发目录 release.sh 刷新位"
        );
        assert_eq!(
            DISTRIBUTABLE_ARTIFACTS,
            ["os-api", "os-api-aarch64"],
            "白名单常量表防漂移"
        );
        match old {
            Some(v) => std::env::set_var("NEXOS_DISTRIBUTABLE_DIR", v),
            None => std::env::remove_var("NEXOS_DISTRIBUTABLE_DIR"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ============ 默认 trait ============

    #[test]
    fn default_trait_is_implemented() {
        fn assert_default<T: Default>() {}
        assert_default::<ProvisioningRouteHandler>();
    }

    // ============ 网络装机 P0（NET_BOOT）============

    #[test]
    fn netboot_filename_whitelist_and_latest_marking() {
        // 合法形态
        let (v, a, id) = parse_repo_iso_filename("ubuntu-26.04.1-live-server-amd64.iso").unwrap();
        assert_eq!(
            (v.as_str(), a.as_str(), id.as_str()),
            ("26.04.1", "amd64", "26.04.1-amd64")
        );
        assert!(parse_repo_iso_filename("ubuntu-24.04.5-live-server-arm64.iso").is_some());
        assert!(parse_repo_iso_filename("ubuntu-26.04-live-server-amd64.iso").is_some());
        // 穿越与异形态全拒
        for bad in [
            "../ubuntu-26.04.1-live-server-amd64.iso",
            "ubuntu-26.04.1-live-server-amd64.iso/../../etc/passwd",
            "..\\ubuntu-26.04.1-live-server-amd64.iso",
            "Ubuntu-26.04.1-live-server-amd64.iso",
            "ubuntu-26.04.1-live-server-i386.iso",
            "ubuntu-26.04.1-desktop-amd64.iso",
            "ubuntu-26.04..1-live-server-amd64.iso",
            "ubuntu--live-server-amd64.iso",
            "ubuntu-26.04.1-live-server-amd64.iso.exe",
            "a".repeat(80).as_str(),
        ] {
            assert!(parse_repo_iso_filename(bad).is_none(), "应拒绝: {bad}");
        }
        // 版本序 + latest 每架构一个
        let files = [
            "ubuntu-26.04.1-live-server-amd64.iso",
            "ubuntu-24.04.5-live-server-amd64.iso",
            "ubuntu-26.04.1-live-server-arm64.iso",
            "ubuntu-26.04.10-live-server-arm64.iso",
        ];
        let mut scanned: Vec<IsoRepoFile> = files
            .iter()
            .map(|f| IsoRepoFile {
                file: f.to_string(),
                version: parse_repo_iso_filename(f).unwrap().0,
                arch: parse_repo_iso_filename(f).unwrap().1,
                id: parse_repo_iso_filename(f).unwrap().2,
                latest: false,
                size_bytes: 1,
                sha256: None,
                sha256_status: "pending".into(),
                extraction_status: "pending".into(),
                extracted: vec![],
                error: None,
            })
            .collect();
        for arch in NETBOOT_ARCHES {
            let best = scanned
                .iter()
                .filter(|f| f.arch == arch)
                .max_by(|x, y| netboot_version_cmp(&x.version, &y.version))
                .map(|f| f.file.clone())
                .unwrap();
            for f in scanned.iter_mut() {
                if f.arch == arch {
                    f.latest = f.file == best;
                }
            }
        }
        let latest: Vec<bool> = scanned.iter().map(|f| f.latest).collect();
        // 26.04.10 > 26.04.1（数值逐段比较，非字典序）
        assert_eq!(latest, vec![true, false, false, true]);
    }

    #[test]
    fn netboot_range_header_parse_variants() {
        let size = 1000u64;
        assert_eq!(parse_range_header(None, size), Ok(HttpRange::Full));
        assert_eq!(
            parse_range_header(Some("bytes=0-1023"), size),
            Ok(HttpRange::Partial {
                start: 0,
                len: 1000
            }) // 尾部 clamp 到文件内
        );
        assert_eq!(
            parse_range_header(Some("bytes=100-199"), size),
            Ok(HttpRange::Partial {
                start: 100,
                len: 100
            })
        );
        assert_eq!(
            parse_range_header(Some("bytes=500-"), size),
            Ok(HttpRange::Partial {
                start: 500,
                len: 500
            })
        );
        assert_eq!(
            parse_range_header(Some("bytes=-100"), size),
            Ok(HttpRange::Partial {
                start: 900,
                len: 100
            }) // 后缀区间
        );
        // 多区间忽略 → Full；坏语法忽略 → Full（RFC 7233 宽松策略）
        assert_eq!(
            parse_range_header(Some("bytes=0-1,5-9"), size),
            Ok(HttpRange::Full)
        );
        assert_eq!(
            parse_range_header(Some("chunks=1-2"), size),
            Ok(HttpRange::Full)
        );
        // 起点越界 / 尾缀 0 → 416
        assert!(parse_range_header(Some("bytes=1000-"), size).is_err());
        assert!(parse_range_header(Some("bytes=-0"), size).is_err());
        // 空文件 + Range → 416；空文件无 Range → Full
        assert!(parse_range_header(Some("bytes=0-"), 0).is_err());
        assert_eq!(parse_range_header(None, 0), Ok(HttpRange::Full));
    }

    #[test]
    fn netboot_resolve_stream_rejects_traversal() {
        // OS ISO 白名单
        assert!(matches!(
            resolve_netboot_stream(Some("ubuntu-26.04.1-live-server-amd64.iso"), None),
            Some(NetbootFileStream::Iso(_))
        ));
        // 引导介质白名单（精确名）
        assert!(matches!(
            resolve_netboot_stream(Some("ipxe.iso"), None),
            Some(NetbootFileStream::BootMedia(_))
        ));
        // casper 提取件：id + 文件名双白名单
        assert!(matches!(
            resolve_netboot_stream(None, Some(("26.04.1-amd64", "vmlinuz"))),
            Some(NetbootFileStream::Casper(_))
        ));
        assert!(matches!(
            resolve_netboot_stream(None, Some(("26.04.1-amd64", "initrd"))),
            Some(NetbootFileStream::Casper(_))
        ));
        // 全部拒绝形态：路径注入 / 异形 id / 异形文件名
        for bad in [
            "../etc/passwd",
            "..%2f..%2fetc%2fpasswd",
            "ubuntu-26.04.1-live-server-amd64.iso/../../x",
            "SHA256SUMS",
            "ipxe.iso/x",
            "bootstrap.ipxe",
        ] {
            assert!(
                resolve_netboot_stream(Some(bad), None).is_none(),
                "isos 应拒: {bad}"
            );
        }
        for bad_id in [
            "../26.04.1-amd64",
            "26.04.1-amd64-x",
            "26;04-amd64",
            "26.04.1-i386",
            "26.04.1-AMD64",
            "-amd64",
            "26.04.1-",
        ] {
            assert!(
                resolve_netboot_stream(None, Some((bad_id, "vmlinuz"))).is_none(),
                "boot id 应拒: {bad_id}"
            );
        }
        for bad_file in ["vmlinuz/../../etc/passwd", "initrd.img", "vmlinuz.exe", ""] {
            assert!(
                resolve_netboot_stream(None, Some(("26.04.1-amd64", bad_file))).is_none(),
                "boot file 应拒: {bad_file}"
            );
        }
        // 双 None / 双 Some → None
        assert!(resolve_netboot_stream(None, None).is_none());
        assert!(resolve_netboot_stream(
            Some("ubuntu-26.04.1-live-server-amd64.iso"),
            Some(("26.04.1-amd64", "vmlinuz"))
        )
        .is_none());
    }

    #[test]
    fn netboot_bootstrap_ipxe_endpoint_snapshot() {
        fake_rt().block_on(async {
            let h = ProvisioningRouteHandler::with_empty();
            let resp = h
                .handle(get_req("/api/v1/provisioning/bootstrap.ipxe"))
                .await
                .unwrap();
            assert_eq!(resp.status, 200);
            assert_eq!(
                resp.headers["content-type"], "text/plain; charset=utf-8",
                "iPXE chain 拉取需原文 text/plain: {resp:?}"
            );
            let script = resp.body.as_str().unwrap();
            // 快照断言：引导链骨架（dhcp → 缺省源 IP → prompt → chain 菜单）
            assert!(script.starts_with("#!ipxe\n"));
            assert!(script.contains("console\n"));
            assert!(script.contains("dhcp net0 || true"));
            assert!(script.contains("set nexos_srv ${next-server}"));
            assert!(script.contains("prompt --key 0x0d 'NexOS IP: ' && read nexos_srv || goto ask"));
            assert!(script.contains(
                "chain --replace http://${nexos_srv}:8558/api/v1/provisioning/ipxe/menu?arch=${buildarch}&platform=${platform} || goto ask"
            ));
        });
    }

    #[test]
    fn netboot_seed_endpoints_render_and_reject() {
        fake_rt().block_on(async {
            let h = ProvisioningRouteHandler::with_empty();
            let host_headers =
                serde_json::json!({"host": "192.0.2.106:8558"});
            // 26.04.1 种子：新式 apt 键 + geoip 关 + late-commands 闭环
            let resp = h
                .handle(get_req_headers(
                    "/api/v1/provisioning/seed/26.04.1-amd64/user-data",
                    host_headers.clone(),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 200, "{resp:?}");
            assert_eq!(resp.headers["content-type"], "text/plain; charset=utf-8");
            let y = resp.body.as_str().unwrap();
            assert!(y.starts_with("#cloud-config\n"));
            assert!(y.contains("    geoip: false"));
            assert!(y.contains("    mirror-selection:"));
            assert!(!y.contains("    primary:\n      - uri"));
            assert!(y.contains("      name: direct"));
            assert!(y.contains("        size: largest"));
            assert!(y.contains("    username: nexos"));
            assert!(y.contains(&format!("    password: '{}'", DEFAULT_SEED_PASSWORD_CRYPT)));
            assert!(y.contains(
                "curtin in-target -- bash -c 'curl -fsSL http://192.0.2.106:8558/api/v1/provisioning/install.sh -o /tmp/nexos-install.sh && bash /tmp/nexos-install.sh --source http://192.0.2.106:8558 --bootstrap 192.0.2.106:7070'"
            ));
            // 24.04.5 种子：旧式 apt 键、无 geoip
            let resp = h
                .handle(get_req_headers(
                    "/api/v1/provisioning/seed/24.04.5-amd64/user-data",
                    host_headers.clone(),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 200);
            let y24 = resp.body.as_str().unwrap();
            assert!(!y24.contains("geoip"));
            assert!(!y24.contains("mirror-selection"));
            assert!(y24.contains("    primary:\n      - uri: http://mirrors.aliyun.com/ubuntu"));
            // arm64 种子可渲染
            let resp = h
                .handle(get_req_headers(
                    "/api/v1/provisioning/seed/26.04.1-arm64/user-data",
                    host_headers.clone(),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 200);
            // meta-data 为空（nocloud 约定）
            let resp = h
                .handle(get_req("/api/v1/provisioning/seed/26.04.1-amd64/meta-data"))
                .await
                .unwrap();
            assert_eq!(resp.status, 200);
            assert_eq!(resp.body.as_str().unwrap(), "");
            // 白名单外 id → 404（穿越/异形）
            for bad in [
                "../26.04.1-amd64",
                "26.04.1-amd64-x",
                "26.04.1-i386",
                "ubuntu-26.04.1",
                "26.04.1-AMD64",
            ] {
                let resp = h
                    .handle(get_req(&format!(
                        "/api/v1/provisioning/seed/{bad}/user-data"
                    )))
                    .await
                    .unwrap();
                assert_eq!(resp.status, 404, "种子应拒: {bad}");
            }
        });
    }

    #[test]
    fn netboot_repo_scan_menu_and_delete_endpoints() {
        // 仓 fixture：两件 amd64（26.04.1 对账 sidecar 正确 + 24.04.5 sidecar 错）
        // + 一件 arm64 + 引导介质；menu 只列 latest（滚动策略）。
        let repo = temp_dir("netboot-repo");
        let isos = repo.join("isos");
        std::fs::create_dir_all(&isos).unwrap();
        let payload: &[u8] = b"FAKE-ISO-CONTENT-FOR-SCAN";
        std::fs::write(isos.join("ubuntu-26.04.1-live-server-amd64.iso"), payload).unwrap();
        std::fs::write(isos.join("ubuntu-24.04.5-live-server-amd64.iso"), payload).unwrap();
        std::fs::write(isos.join("ubuntu-26.04.1-live-server-arm64.iso"), payload).unwrap();
        std::fs::write(isos.join("ipxe.iso"), b"I PXE").unwrap();
        let good_sha = format!("{}", {
            use sha2::Digest as _;
            let mut hst = sha2::Sha256::new();
            hst.update(payload);
            format!("{:x}", hst.finalize())
        });
        std::fs::write(
            isos.join("ubuntu-26.04.1-live-server-amd64.iso.sha256"),
            format!("{good_sha}  ubuntu-26.04.1-live-server-amd64.iso\n"),
        )
        .unwrap();
        std::fs::write(
            isos.join("ubuntu-24.04.5-live-server-amd64.iso.sha256"),
            format!("{}  ubuntu-24.04.5-live-server-amd64.iso\n", "f".repeat(64)),
        )
        .unwrap();

        let _guard = NETBOOT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var("NEXOS_PROVISION_REPO").ok();
        std::env::set_var("NEXOS_PROVISION_REPO", &repo);
        fake_rt().block_on(async {
            let h = ProvisioningRouteHandler::with_empty();
            let host_headers = serde_json::json!({"host": "10.0.0.9:8558"});
            // 1) GET /isos：现场扫描（未 scan 过也可见），latest 标记 + sidecar 状态
            let resp = h
                .handle(get_req("/api/v1/provisioning/isos"))
                .await
                .unwrap();
            assert_eq!(resp.status, 200, "{resp:?}");
            let files = resp.body["files"].as_array().unwrap();
            assert_eq!(files.len(), 3, "OS 件 3 件（ipxe.iso 不入 OS 清单）");
            let amd_latest = files
                .iter()
                .find(|f| f["file"] == "ubuntu-26.04.1-live-server-amd64.iso")
                .unwrap();
            assert_eq!(amd_latest["latest"], serde_json::json!(true));
            assert_eq!(amd_latest["sha256_status"], "sidecar");
            assert_eq!(amd_latest["version"], "26.04.1");
            let old_amd = files
                .iter()
                .find(|f| f["file"] == "ubuntu-24.04.5-live-server-amd64.iso")
                .unwrap();
            assert_eq!(old_amd["latest"], serde_json::json!(false), "老版不 latest");

            // 2) GET /ipxe/menu?arch=amd64：只列最新稳定版（26.04.1）
            let resp = h
                .handle(get_req_headers(
                    "/api/v1/provisioning/ipxe/menu?arch=x86_64&platform=efi",
                    host_headers.clone(),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 200);
            let menu = resp.body.as_str().unwrap();
            assert!(menu.contains("menu NexOS 网络装机（efi/amd64）"));
            assert!(menu.contains("Ubuntu Server 26.04.1（amd64）"));
            assert!(!menu.contains("24.04"), "老版本不进自动装机菜单");
            assert!(menu.contains("item --disabled clonezilla"), "Clonezilla 占位");
            assert!(menu.contains(
                "kernel http://10.0.0.9:8558/api/v1/provisioning/boot/26.04.1-amd64/vmlinuz root=/dev/ram0 ramdisk_size=1500000 ip=dhcp url=http://10.0.0.9:8558/api/v1/provisioning/isos/ubuntu-26.04.1-live-server-amd64.iso autoinstall ds=nocloud-net\\;s=http://10.0.0.9:8558/api/v1/provisioning/seed/26.04.1-amd64/"
            ));
            assert!(menu.contains("initrd http://10.0.0.9:8558/api/v1/provisioning/boot/26.04.1-amd64/initrd"));
            // arm64 菜单走 arm64 件
            let resp = h
                .handle(get_req_headers(
                    "/api/v1/provisioning/ipxe/menu?arch=arm64&platform=efi",
                    host_headers.clone(),
                ))
                .await
                .unwrap();
            assert!(resp.body.as_str().unwrap().contains("Ubuntu Server 26.04.1（arm64）"));
            // 未知架构缺省 amd64；仓空架构提示（此处 amd64 有货，仅形态断言）
            let resp = h
                .handle(get_req("/api/v1/provisioning/ipxe/menu"))
                .await
                .unwrap();
            assert!(resp.body.as_str().unwrap().contains("/amd64）"));

            // 3) POST /isos/scan：登记 + 后台对账（good → verified；bad → mismatch）
            let resp = h
                .handle(post_req("/api/v1/provisioning/isos/scan", serde_json::Value::Null))
                .await
                .unwrap();
            assert_eq!(resp.status, 200, "{resp:?}");
            assert_eq!(resp.body["scanned"], serde_json::json!(3));
            // 轮询后台任务终态（伪 ISO 提取会失败——只关心 sha256 对账结果）
            let deadline = Instant::now() + Duration::from_secs(15);
            let (mut good_ok, mut bad_ok) = (false, false);
            while Instant::now() < deadline {
                let resp = h.handle(get_req("/api/v1/provisioning/isos")).await.unwrap();
                let files = resp.body["files"].as_array().unwrap().clone();
                let g = files
                    .iter()
                    .find(|f| f["file"] == "ubuntu-26.04.1-live-server-amd64.iso")
                    .unwrap()
                    .clone();
                let b = files
                    .iter()
                    .find(|f| f["file"] == "ubuntu-24.04.5-live-server-amd64.iso")
                    .unwrap()
                    .clone();
                good_ok = g["sha256_status"] == "verified";
                bad_ok = b["sha256_status"] == "mismatch";
                if good_ok && bad_ok {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            assert!(good_ok, "26.04.1 实算应与 sidecar 一致（verified）");
            assert!(bad_ok, "24.04.5 错 sidecar 应报 mismatch");
            // mismatch 件不可用：提取失败 + error 说明
            let resp = h.handle(get_req("/api/v1/provisioning/isos")).await.unwrap();
            let b = resp.body["files"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["file"] == "ubuntu-24.04.5-live-server-amd64.iso")
                .unwrap();
            assert_eq!(b["extraction_status"], "failed");
            assert!(b["error"].as_str().unwrap().contains("sha256 不符"));

            // 4) DELETE /isos/:file：滚动删旧版（ISO + sidecar + boot 目录）
            let resp = h
                .handle(del_req(
                    "/api/v1/provisioning/isos/ubuntu-24.04.5-live-server-amd64.iso",
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 200, "{resp:?}");
            assert!(!isos.join("ubuntu-24.04.5-live-server-amd64.iso").exists());
            assert!(!isos.join("ubuntu-24.04.5-live-server-amd64.iso.sha256").exists());
            // 白名单外文件名 → 400；不存在 → 404
            let resp = h
                .handle(del_req("/api/v1/provisioning/isos/..%2F..%2Fetc%2Fpasswd"))
                .await
                .unwrap();
            assert_eq!(resp.status, 400);
            let resp = h
                .handle(del_req(
                    "/api/v1/provisioning/isos/ubuntu-99.99-live-server-amd64.iso",
                ))
                .await
                .unwrap();
            assert_eq!(resp.status, 404);
        });
        match old {
            Some(v) => std::env::set_var("NEXOS_PROVISION_REPO", v),
            None => std::env::remove_var("NEXOS_PROVISION_REPO"),
        }
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn netboot_scan_makes_boot_dir_extraction_entries() {
        // 纯函数侧：verify_and_extract_iso_sync 对伪 ISO（非 ISO9660）报
        // "不是 ISO"类失败且不动 boot 目录外文件——真实提取已由
        // os-provision netboot_real 对真机 ISO 验证。
        let repo = temp_dir("netboot-extract");
        let isos = repo.join("isos");
        std::fs::create_dir_all(&isos).unwrap();
        std::fs::write(
            isos.join("ubuntu-26.04.1-live-server-amd64.iso"),
            b"NOT AN ISO",
        )
        .unwrap();
        let st = IsoRepoFile {
            file: "ubuntu-26.04.1-live-server-amd64.iso".into(),
            version: "26.04.1".into(),
            arch: "amd64".into(),
            id: "26.04.1-amd64".into(),
            latest: true,
            size_bytes: 10,
            sha256: None,
            sha256_status: "pending".into(),
            extraction_status: "pending".into(),
            extracted: vec![],
            error: None,
        };
        let _guard = NETBOOT_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old = std::env::var("NEXOS_PROVISION_REPO").ok();
        std::env::set_var("NEXOS_PROVISION_REPO", &repo);
        let out = verify_and_extract_iso_sync(&st);
        // sha256 实算成功（写 sidecar）；提取失败（非 ISO）
        assert_eq!(out.sha256_status, "verified");
        assert!(out.sha256.is_some());
        assert!(isos
            .join("ubuntu-26.04.1-live-server-amd64.iso.sha256")
            .is_file());
        assert_eq!(out.extraction_status, "failed");
        assert!(out.error.as_deref().unwrap_or("").contains("ISO"));
        match old {
            Some(v) => std::env::set_var("NEXOS_PROVISION_REPO", v),
            None => std::env::remove_var("NEXOS_PROVISION_REPO"),
        }
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn netboot_query_params_parses_arch_platform() {
        let (a, p) = query_params("/api/v1/provisioning/ipxe/menu?arch=arm64&platform=efi");
        assert_eq!(a.as_deref(), Some("arm64"));
        assert_eq!(p.as_deref(), Some("efi"));
        let (a, p) = query_params("/api/v1/provisioning/ipxe/menu");
        assert!(a.is_none() && p.is_none());
        assert_eq!(map_ipxe_buildarch("x86_64"), "amd64");
        assert_eq!(map_ipxe_buildarch("arm64"), "arm64");
    }
}
