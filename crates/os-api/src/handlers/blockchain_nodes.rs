//! 节点运行管理（geth / bitcoind 真实子进程）—— `blockchain.rs` 的子模块。
//!
//! 定位：blockchain.rs 的 `/api/v1/blockchain/nodes` 是 **docker-compose 编排**
//! （构造 yaml + spawn docker）；本模块补上**本机二进制直跑**的节点生命周期
//! （实例管理成熟模式：spawn 进程表 + 按实例日志文件 + 状态修正），服务
//! ETH 主网/Sepolia/dev（geth）与 BTC 主网/testnet/regtest（bitcoind）。
//!
//! # 模式语义（2026-09 调研定稿，docs/BLOCKCHAIN_NODES.md §调研数据表）
//!
//! | 链 | fast | full |
//! |----|------|------|
//! | ethereum | geth `--syncmode snap --gcmode full`（默认剪枝；主网约 700GB） | `--gcmode archive`（主网约 2.2TB，v1.16+ path-based） |
//! | bitcoin | `prune=550`（约 12GB；仍需全量下载+验证，网络流量≈链体积） | 无 prune（主网约 750GB；可选 `txindex=1` 再 +160GB） |
//!
//! 诚实口径：geth LES 轻客户端已移除（PoS 后不可用）——ETH"快速模式"没有
//! 百 GB 以下的本地方案；主网 fast 仍需 X00GB 级磁盘与 1-3 天同步。
//! 主网/Sepolia 还需共识客户端（CL）跟链头（见 presets 的
//! `requires_consensus_client` + 文档 §CL 搭配指引）。
//!
//! # 空间预检（用户点名）
//!
//! 内置各链 full 模式**预估体积表**（GB，调研数字，随链增长会被推翻——env
//! `NEXOS_CHAIN_NODE_SIZE_HINTS` JSON 可覆盖，键 `"<kind>/<network>/<mode>"`）。
//! 选 full 时检查 datadir 所在文件系统可用空间（`df -B1 <dir>`，monitor.rs
//! read_root_disk 同款口径）：不足 → 409 禁止创建/启动（需要 X 可用 Y）；
//! fast 也显示预估（不阻断，warning 透传）。
//!
//! # 路由表（9 条，前缀 `/api/v1/blockchain/chain-nodes`，component=blockchain）
//!
//! | method | path | 动作 |
//! |--------|------|------|
//! | GET    | ``（列表） | 列节点（含进程状态修正：running 但 pid 已死 → stopped） |
//! | POST   | `` | 创建节点（admin；full 模式空间预检不过 → 409） |
//! | GET    | `/presets` | 链/网络/模式预设 + 预估体积 + 二进制探测 |
//! | GET    | `/space-check` | 空间预检（?kind=&network=&mode=&data_dir=&txindex=） |
//! | GET    | `/:id` | 节点详情 |
//! | POST   | `/:id/start` | 启动（admin；二进制缺失/空间不足 → 409 附安装指引） |
//! | POST   | `/:id/stop` | 停止（admin；SIGTERM → 10s → SIGKILL 兜底） |
//! | DELETE | `/:id` | 删除（admin；链数据目录**不**自动删） |
//! | GET    | `/:id/logs` | 日志尾部（?tail=200，默认 200 行） |
//!
//! # 持久化与进程语义
//!
//! - SQLite 表 `blockchain_nodes`（env `NEXOS_CHAIN_NODES_DB`，缺省
//!   `/tank/os-data/chain-nodes.db`，/tank 不存在时回退 `./chain-nodes.db`）。
//! - 重启恢复：**status 一律 stopped、pid 清空**（llm_instances 同款——旧 pid
//!   不可信，不自动恢复运行态，由用户经 UI 重新启动）。
//! - spawn：stdout+stderr 追加写 `<log_dir>/<id>.log`（env
//!   `NEXOS_CHAIN_NODE_LOG_DIR`，缺省 `/tank/os-data/chain-node-logs`，回退
//!   `./chain-node-logs`）；Child 句柄存进程表（`Arc<Mutex<Option<Child>>>`，
//!   监测任务 try_wait 收尸防僵尸 + 退出回写 error；stop 走 SIGTERM→等待→
//!   SIGKILL）；spawn 后 30s 后台监测（geth 经 `eth_syncing` 修正
//!   syncing→running，bitcoind 经 RPC 端口 TCP 探活）。
//! - **绝不自动启动主网同步**：本模块只落记录与进程编排，几百 GB/数天的
//!   实际同步由用户经 UI 点击 start 触发。
//!
//! # 客户端安装（不在节点内自动 apt）
//!
//! 二进制解析顺序：env `NEXOS_CHAIN_NODE_BIN_GETH`/`NEXOS_CHAIN_NODE_BIN_BITCOIND`
//! → PATH 扫描（文件系统判 exe，不 spawn）→ 常规路径（/usr/local/bin、
//! /usr/bin、~/.local/bin、~/bin）。缺失时 start 返回 409 + 安装指引
//! （geth：x86_64 PPA / aarch64 官方 tar.gz；bitcoind：Ubuntu universe apt /
//! bitcoincore.org aarch64 tar.gz——详见 docs/BLOCKCHAIN_NODES.md §安装指引）。

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use once_cell::sync::Lazy;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use crate::error::ApiGatewayError;
use crate::gateway::{ApiResponse, HttpMethod, RouteSpec};

/// RPC 探测共享 reqwest Client（eth_syncing / bitcoind 端口探活）。
static HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
});

// ----------------------------------------------------------------------------
// 常量与 env
// ----------------------------------------------------------------------------

/// DB 文件路径覆盖 env。
pub const ENV_DB: &str = "NEXOS_CHAIN_NODES_DB";

/// 节点日志目录覆盖 env。
pub const ENV_LOG_DIR: &str = "NEXOS_CHAIN_NODE_LOG_DIR";

/// 预估体积表覆盖 env（JSON：{"ethereum/mainnet/fast": 700, ...}，单位 GB）。
pub const ENV_SIZE_HINTS: &str = "NEXOS_CHAIN_NODE_SIZE_HINTS";

/// 节点数据根目录覆盖 env（默认 datadir 基址，见 [`default_data_root`]）。
pub const ENV_DATA_ROOT: &str = "NEXOS_CHAIN_NODE_DATA_ROOT";

/// 二进制路径覆盖 env 前缀（NEXOS_CHAIN_NODE_BIN_GETH / _BITCOIND）。
const ENV_BIN_PREFIX: &str = "NEXOS_CHAIN_NODE_BIN_";

/// 日志尾部默认行数。
const LOG_TAIL_DEFAULT: usize = 200;

/// 日志尾部上限（行）。
const LOG_TAIL_MAX: usize = 2000;

/// spawn 后监测轮次 × 间隔（6×5s=30s 窗口；此后靠读取时状态修正兜底）。
const SPAWN_MONITOR_POLLS: u32 = 6;
const SPAWN_MONITOR_INTERVAL: Duration = Duration::from_secs(5);

/// stop 优雅等待（SIGTERM 后最多 10s 再 SIGKILL）。
const STOP_GRACE_SECS: u64 = 10;

/// txindex 追加体积（GB，bitcoin full 可选索引）。
const TXINDEX_EXTRA_GB: u64 = 160;

// ----------------------------------------------------------------------------
// 预设（链/网络/模式 + 预估体积；纯数据，2026-09 调研口径）
// ----------------------------------------------------------------------------

/// 单一运行模式的说明（fast/full 二选一）。
#[derive(Debug, Clone, Serialize)]
pub struct NodeModeInfo {
    pub mode: String,
    /// 语义标签（如 "snap + 默认剪枝"）。
    pub label: String,
    /// 生成的关键旗标摘要（供前端展示）。
    pub flags: String,
    /// 预估落盘体积（GB；估算口径见文档，会随链增长）。
    pub estimated_size_gb: u64,
    /// 同步时长粗估（人读字符串）。
    pub sync_estimate: String,
    /// 诚实备注（如 prune 仍需全量下载）。
    pub note: Option<String>,
}

/// 链+网络预设（GET /presets 元素）。
#[derive(Debug, Clone, Serialize)]
pub struct ChainNetworkPreset {
    pub kind: String,
    pub network: String,
    pub name: String,
    pub default_client: String,
    pub default_rpc_port: u16,
    pub default_p2p_port: u16,
    pub modes: Vec<NodeModeInfo>,
    /// post-merge ETH 网络需共识客户端（CL）才能跟链头（EL-only 会停在 merge 高度）。
    pub requires_consensus_client: bool,
    /// 该预设客户端二进制是否已检出（presets 端点现场填充）。
    pub binary_installed: bool,
    /// 安装指引（缺失时展示；端点现场填充）。
    pub install_hint: String,
}

/// 内置预估体积表键 `"<kind>/<network>/<mode>"` → GB（full 为含余量的规划值）。
///
/// 口径（2026-09 调研，docs/BLOCKCHAIN_NODES.md §1）：
/// - ETH 主网 snap+剪枝实测口径 650-700GB（规划 700）；archive（v1.16+
///   path-based）约 2TB（规划 2200；旧 hash-based 14TB+ 不支持）。
/// - Sepolia 测试网 spam 波动大，估算 120/300。
/// - BTC prune=550 约 9-11GB（规划 12）；BTC 主网全节点链体积 2026 现值
///   700-800GB（规划 750）；txindex ≈ +160GB。
#[must_use]
pub fn builtin_size_hints() -> HashMap<String, u64> {
    HashMap::from([
        ("ethereum/mainnet/fast".to_string(), 700),
        ("ethereum/mainnet/full".to_string(), 2200),
        ("ethereum/sepolia/fast".to_string(), 120),
        ("ethereum/sepolia/full".to_string(), 300),
        ("ethereum/dev/fast".to_string(), 1),
        ("ethereum/dev/full".to_string(), 1),
        ("bitcoin/mainnet/fast".to_string(), 12),
        ("bitcoin/mainnet/full".to_string(), 750),
        ("bitcoin/testnet/fast".to_string(), 12),
        ("bitcoin/testnet/full".to_string(), 350),
        ("bitcoin/regtest/fast".to_string(), 1),
        ("bitcoin/regtest/full".to_string(), 1),
    ])
}

/// 预估体积解析：覆盖表 → 内置表（未知键 1GB 兜底——dev/regtest 级）。
#[must_use]
pub fn resolve_size_gb(
    overrides: &HashMap<String, u64>,
    kind: &str,
    network: &str,
    mode: &str,
) -> u64 {
    let key = format!("{kind}/{network}/{mode}");
    overrides
        .get(&key)
        .copied()
        .unwrap_or_else(|| builtin_size_hints().get(&key).copied().unwrap_or(1))
}

/// 内置网络预设（binary_installed / install_hint 由 presets 端点现场填充）。
#[must_use]
pub fn network_presets() -> Vec<ChainNetworkPreset> {
    let empty = HashMap::new();
    vec![
        ChainNetworkPreset {
            kind: "ethereum".into(),
            network: "mainnet".into(),
            name: "Ethereum 主网".into(),
            default_client: "geth".into(),
            default_rpc_port: 8545,
            default_p2p_port: 30303,
            modes: vec![
                NodeModeInfo {
                    mode: "fast".into(),
                    label: "快速（snap + 默认剪枝）".into(),
                    flags: "--syncmode snap --gcmode full".into(),
                    estimated_size_gb: resolve_size_gb(&empty, "ethereum", "mainnet", "fast"),
                    sync_estimate: "1-3 天（NVMe SSD + 足够带宽）".into(),
                    note: Some(
                        "LES 轻客户端已移除：主网没有百 GB 以下本地方案，fast 仍需 ~700GB、\
                         全量下载验证；仅最近 128 个状态可查（历史状态查询需 archive）。"
                            .into(),
                    ),
                },
                NodeModeInfo {
                    mode: "full".into(),
                    label: "全节点（archive 存档）".into(),
                    flags: "--syncmode snap --gcmode archive".into(),
                    estimated_size_gb: resolve_size_gb(&empty, "ethereum", "mainnet", "full"),
                    sync_estimate: "数天".into(),
                    note: Some(
                        "geth v1.16+ path-based archive 约 2.2TB（旧 hash-based 14TB+ 不建议）。\
                         需另配共识客户端（CL）。"
                            .into(),
                    ),
                },
            ],
            requires_consensus_client: true,
            binary_installed: false,
            install_hint: String::new(),
        },
        ChainNetworkPreset {
            kind: "ethereum".into(),
            network: "sepolia".into(),
            name: "Sepolia 测试网".into(),
            default_client: "geth".into(),
            default_rpc_port: 8555,
            default_p2p_port: 30304,
            modes: vec![
                NodeModeInfo {
                    mode: "fast".into(),
                    label: "快速（snap + 默认剪枝）".into(),
                    flags: "--sepolia --syncmode snap --gcmode full".into(),
                    estimated_size_gb: resolve_size_gb(&empty, "ethereum", "sepolia", "fast"),
                    sync_estimate: "数小时".into(),
                    note: Some(
                        "测试网 spam 波动大，120GB 估算会随时被推翻；ethPandaOps 提供\
                         预同步快照可缩短到 1 小时内（文档 §2）。"
                            .into(),
                    ),
                },
                NodeModeInfo {
                    mode: "full".into(),
                    label: "全节点（archive 存档）".into(),
                    flags: "--sepolia --syncmode snap --gcmode archive".into(),
                    estimated_size_gb: resolve_size_gb(&empty, "ethereum", "sepolia", "full"),
                    sync_estimate: "1 天+".into(),
                    note: Some("需另配共识客户端（CL）。".into()),
                },
            ],
            requires_consensus_client: true,
            binary_installed: false,
            install_hint: String::new(),
        },
        ChainNetworkPreset {
            kind: "ethereum".into(),
            network: "dev".into(),
            name: "ETH 本地开发链（--dev）".into(),
            default_client: "geth".into(),
            default_rpc_port: 8546,
            default_p2p_port: 30305,
            modes: vec![NodeModeInfo {
                mode: "fast".into(),
                label: "快速（--dev 即时出块）".into(),
                flags: "--dev".into(),
                estimated_size_gb: 1,
                sync_estimate: "秒级".into(),
                note: Some("本地 PoA 开发链，无共识客户端需求，不连外网。".into()),
            }],
            requires_consensus_client: false,
            binary_installed: false,
            install_hint: String::new(),
        },
        ChainNetworkPreset {
            kind: "bitcoin".into(),
            network: "mainnet".into(),
            name: "Bitcoin 主网".into(),
            default_client: "bitcoind".into(),
            default_rpc_port: 8332,
            default_p2p_port: 8333,
            modes: vec![
                NodeModeInfo {
                    mode: "fast".into(),
                    label: "快速（prune=550 剪枝）".into(),
                    flags: "prune=550".into(),
                    estimated_size_gb: resolve_size_gb(&empty, "bitcoin", "mainnet", "fast"),
                    sync_estimate: "6-24 小时".into(),
                    note: Some(
                        "完全验证节点：仍需**全量下载并验证整条链**（网络流量≈700GB），\
                         仅旧区块落盘后即删；prune 与 txindex 互斥。"
                            .into(),
                    ),
                },
                NodeModeInfo {
                    mode: "full".into(),
                    label: "全节点（保留全部区块，可选 txindex）".into(),
                    flags: "（无 prune；可选 txindex=1）".into(),
                    estimated_size_gb: resolve_size_gb(&empty, "bitcoin", "mainnet", "full"),
                    sync_estimate: "1-3 天".into(),
                    note: Some(format!(
                        "链体积 2026 现值 700-800GB 且年增 50-80GB；勾选 txindex 再 +{TXINDEX_EXTRA_GB}GB。"
                    )),
                },
            ],
            requires_consensus_client: false,
            binary_installed: false,
            install_hint: String::new(),
        },
        ChainNetworkPreset {
            kind: "bitcoin".into(),
            network: "testnet".into(),
            name: "Bitcoin 测试网（testnet3）".into(),
            default_client: "bitcoind".into(),
            default_rpc_port: 18332,
            default_p2p_port: 18333,
            modes: vec![
                NodeModeInfo {
                    mode: "fast".into(),
                    label: "快速（prune=550 剪枝）".into(),
                    flags: "testnet=1 + prune=550".into(),
                    estimated_size_gb: resolve_size_gb(&empty, "bitcoin", "testnet", "fast"),
                    sync_estimate: "数小时".into(),
                    note: Some("prune 上限与主网同（~12GB），但初始下载仍走整条测试链。".into()),
                },
                NodeModeInfo {
                    mode: "full".into(),
                    label: "全节点".into(),
                    flags: "testnet=1（无 prune）".into(),
                    estimated_size_gb: resolve_size_gb(&empty, "bitcoin", "testnet", "full"),
                    sync_estimate: "1 天".into(),
                    note: Some("testnet3 历史长且 spam 多，全量约 350GB。".into()),
                },
            ],
            requires_consensus_client: false,
            binary_installed: false,
            install_hint: String::new(),
        },
        ChainNetworkPreset {
            kind: "bitcoin".into(),
            network: "regtest".into(),
            name: "Bitcoin 本地回归网（regtest）".into(),
            default_client: "bitcoind".into(),
            default_rpc_port: 18443,
            default_p2p_port: 18444,
            modes: vec![NodeModeInfo {
                mode: "fast".into(),
                label: "快速（本地即时链）".into(),
                flags: "regtest=1".into(),
                estimated_size_gb: 1,
                sync_estimate: "秒级".into(),
                note: Some("本地回归测试网，不连外网，秒级起停（冒烟/开发用）。".into()),
            }],
            requires_consensus_client: false,
            binary_installed: false,
            install_hint: String::new(),
        },
    ]
}

/// geth 安装指引（x86_64 PPA / aarch64 官方 tar.gz）。
#[must_use]
pub fn geth_install_hint() -> String {
    "安装 geth：\n\
     x86_64: sudo add-apt-repository -y ppa:ethereum/ethereum && sudo apt update && sudo apt install ethereum\n\
     aarch64: 从 https://geth.ethereum.org/downloads 下载 geth-linux-arm64-<ver>.tar.gz，\
     解压后 sudo install -m 0755 geth /usr/local/bin/（PPA 无 arm64 构建）\n\
     或用 env NEXOS_CHAIN_NODE_BIN_GETH=<路径> 指定已装二进制"
        .to_string()
}

/// bitcoind 安装指引。
#[must_use]
pub fn bitcoind_install_hint() -> String {
    "安装 bitcoind：\n\
     x86_64: sudo apt install bitcoind（Ubuntu universe，版本较旧）或 \
     sudo add-apt-repository -y ppa:bitcoin/bitcoin && sudo apt update && sudo apt install bitcoind\n\
     aarch64: 从 https://bitcoincore.org/en/download/ 下载 bitcoin-<ver>-aarch64-linux-gnu.tar.gz，\
     校验 SHA256SUMS 后 sudo install -m 0755 -t /usr/local/bin bitcoin-<ver>/bin/*（PPA 无 arm64）\n\
     或用 env NEXOS_CHAIN_NODE_BIN_BITCOIND=<路径> 指定已装二进制"
        .to_string()
}

// ----------------------------------------------------------------------------
// 二进制解析（纯文件系统探测，不 spawn）
// ----------------------------------------------------------------------------

/// 判断路径可执行（存在 + 是文件 + 任一 x 位）。
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(m) => m.is_file() && (m.permissions().mode() & 0o111 != 0),
        Err(_) => false,
    }
}

/// PATH 环境变量拆分（空 PATH → 空 vec）。
fn path_dirs() -> Vec<String> {
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// 家目录（$HOME → /home/$USER → /root）。
fn home_dir() -> String {
    if let Some(h) = std::env::var("HOME").ok().filter(|v| !v.trim().is_empty()) {
        return h;
    }
    if let Some(u) = std::env::var("USER").ok().filter(|v| v != "root" && !v.is_empty()) {
        return format!("/home/{u}");
    }
    "/root".to_string()
}

/// 解析客户端二进制路径：注入覆盖 → env `NEXOS_CHAIN_NODE_BIN_<CLIENT>` →
/// PATH 扫描 → 常规路径。找不到返回 None。纯文件系统探测（不 spawn
/// `command -v`），测试可注入覆盖避免碰宿主环境。
#[must_use]
pub fn resolve_client_bin(client: &str, overrides: &HashMap<String, String>) -> Option<String> {
    if let Some(p) = overrides
        .get(client)
        .cloned()
        .or_else(|| {
            let env_key = format!("{ENV_BIN_PREFIX}{}", client.to_ascii_uppercase());
            std::env::var(&env_key).ok()
        })
        .filter(|v| !v.trim().is_empty())
    {
        let p = p.trim().to_string();
        return if is_executable(Path::new(&p)) { Some(p) } else { None };
    }
    let mut candidates: Vec<String> = path_dirs()
        .iter()
        .map(|d| format!("{d}/{client}"))
        .collect();
    for d in ["/usr/local/bin", "/usr/bin"] {
        let c = format!("{d}/{client}");
        if !candidates.contains(&c) {
            candidates.push(c);
        }
    }
    let home = home_dir();
    candidates.push(format!("{home}/.local/bin/{client}"));
    candidates.push(format!("{home}/bin/{client}"));
    candidates
        .into_iter()
        .find(|c| is_executable(Path::new(c)))
}

// ----------------------------------------------------------------------------
// 磁盘空间预检（df -B1，monitor.rs read_root_disk 同款口径）
// ----------------------------------------------------------------------------

/// `df -B1 <path>` 解析为 (filesystem, total, used, available) 字节；失败 → None。
///
/// 第 1 列 filesystem、第 2 列 total、第 3 列 used、第 4 列 available（-B1 →
/// 字节）。调用方需先确保 path 存在（df 对不存在路径报错）。
pub(crate) fn df_bytes(path: &str) -> Option<(String, u64, u64, u64)> {
    let out = Command::new("df").args(["-B1", path]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().nth(1)?;
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 4 {
        return None;
    }
    Some((
        parts[0].to_string(),
        parts[1].parse().ok()?,
        parts[2].parse().ok()?,
        parts[3].parse().ok()?,
    ))
}

/// 空间预检结果。
#[derive(Debug, Clone, Serialize)]
pub struct SpaceCheck {
    pub kind: String,
    pub network: String,
    pub mode: String,
    pub data_dir: String,
    /// 预估所需（字节；含 txindex 追加）。
    pub required_bytes: u64,
    /// 预估所需（GB，人读口径）。
    pub required_gb: u64,
    /// datadir 所在文件系统可用（字节；探测失败 0）。
    pub available_bytes: u64,
    /// 是否充足（full 模式阻断判据；fast 展示用同字段）。
    pub sufficient: bool,
    /// 是否阻断（full=true；fast 恒 false）。
    pub blocking: bool,
    /// datadir 所在文件系统（df 第 1 列）。
    pub filesystem: String,
    pub error: Option<String>,
}

/// 计算预估所需字节（GB→GiB ×1024³；txindex 追加）。
#[must_use]
pub fn required_bytes_for(
    overrides: &HashMap<String, u64>,
    kind: &str,
    network: &str,
    mode: &str,
    txindex: bool,
) -> u64 {
    let gb =
        resolve_size_gb(overrides, kind, network, mode) + u64::from(txindex) * TXINDEX_EXTRA_GB;
    gb * 1024 * 1024 * 1024
}

/// 空间预检（阻塞 IO：create_dir_all + df；调用方在 spawn_blocking 或测试直呼）。
pub(crate) fn space_check_blocking(
    overrides: &HashMap<String, u64>,
    kind: &str,
    network: &str,
    mode: &str,
    data_dir: &str,
    txindex: bool,
) -> SpaceCheck {
    let required = required_bytes_for(overrides, kind, network, mode, txindex);
    let mut sc = SpaceCheck {
        kind: kind.to_string(),
        network: network.to_string(),
        mode: mode.to_string(),
        data_dir: data_dir.to_string(),
        required_bytes: required,
        required_gb: required / (1024 * 1024 * 1024),
        available_bytes: 0,
        sufficient: false,
        blocking: mode == "full",
        filesystem: String::new(),
        error: None,
    };
    if let Err(e) = std::fs::create_dir_all(data_dir) {
        sc.error = Some(format!("创建数据目录 {data_dir} 失败: {e}"));
        return sc;
    }
    match df_bytes(data_dir) {
        Some((fs, _total, _used, avail)) => {
            sc.available_bytes = avail;
            sc.sufficient = avail >= required;
            sc.filesystem = fs;
        }
        None => {
            sc.error = Some("df 探测失败（无法确定可用空间）".to_string());
        }
    }
    sc
}

/// 节点数据根目录（默认 datadir = `<root>/<network>`）。
///
/// 优先级：env `NEXOS_CHAIN_NODE_DATA_ROOT` → `/tank/blockchain`（若可创建/
/// 可写）→ `/tank/os-data/blockchain`（同 tank 文件系统，os-api 服务用户
/// oem 可写——106 实测 /tank 为 root 属主不可建）→ `./blockchain-nodes-data`。
#[must_use]
pub fn default_data_root() -> String {
    if let Some(r) = std::env::var(ENV_DATA_ROOT).ok().filter(|v| !v.trim().is_empty()) {
        return r.trim().to_string();
    }
    for cand in ["/tank/blockchain", "/tank/os-data/blockchain"] {
        if std::fs::create_dir_all(cand).is_ok() || dir_writable(cand) {
            return cand.to_string();
        }
    }
    "./blockchain-nodes-data".to_string()
}

/// 目录存在且可写（touch 探测）。
fn dir_writable(dir: &str) -> bool {
    let probe = std::path::Path::new(dir).join(".nexos-write-probe");
    std::fs::write(&probe, b"").is_ok()
        && std::fs::remove_file(&probe).is_ok()
}

// ----------------------------------------------------------------------------
// DTO 与落盘结构
// ----------------------------------------------------------------------------

/// 节点记录（API 视图 = 落盘行，无私密字段）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainNode {
    pub id: String,
    pub name: String,
    /// `ethereum` / `bitcoin`。
    pub kind: String,
    /// eth: `mainnet|sepolia|dev`；btc: `mainnet|testnet|regtest`。
    pub network: String,
    /// `geth` / `bitcoind`。
    pub client: String,
    /// `fast` / `full`。
    pub mode: String,
    pub data_dir: String,
    pub rpc_port: u16,
    pub p2p_port: u16,
    /// 仅 bitcoin full 可选（与 prune 互斥）。
    pub txindex: bool,
    /// 附加旗标（空白拆分，argv 数组直传无注入面）。
    pub extra_flags: String,
    /// `stopped|running|syncing|error`。
    pub status: String,
    pub pid: Option<u32>,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub last_started_at: Option<String>,
    /// 最近一次真实拉起命令（argv 单行）。
    pub last_command: Option<String>,
}

/// 创建节点请求体。
#[derive(Debug, Deserialize)]
struct CreateNodeBody {
    name: String,
    kind: String,
    network: String,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    client: Option<String>,
    #[serde(default)]
    data_dir: Option<String>,
    #[serde(default)]
    rpc_port: Option<u16>,
    #[serde(default)]
    p2p_port: Option<u16>,
    #[serde(default)]
    txindex: Option<bool>,
    #[serde(default)]
    extra_flags: Option<String>,
}

// ----------------------------------------------------------------------------
// 命令构造（纯函数，可单测）
// ----------------------------------------------------------------------------

/// 构造 geth 启动 argv（network: mainnet/sepolia/dev；mode: fast/full）。
///
/// - mainnet 无网络旗标（geth 缺省）；sepolia `--sepolia`；dev `--dev`。
/// - fast：`--syncmode snap --gcmode full`；full：`--gcmode archive`。
/// - HTTP RPC 绑 127.0.0.1（不对外暴露），API 面 eth/net/web3/txpool。
/// - extra_flags 空白拆分后**追加**（geth 同名旗标后者生效，可覆盖默认）。
#[must_use]
pub fn build_geth_argv(node: &ChainNode) -> Vec<String> {
    let mut argv = vec!["geth".to_string()];
    match node.network.as_str() {
        "sepolia" => argv.push("--sepolia".into()),
        "dev" => argv.push("--dev".into()),
        _ => {}
    }
    argv.push("--datadir".into());
    argv.push(node.data_dir.clone());
    argv.push("--port".into());
    argv.push(node.p2p_port.to_string());
    argv.push("--http".into());
    argv.push("--http.addr".into());
    argv.push("127.0.0.1".into());
    argv.push("--http.port".into());
    argv.push(node.rpc_port.to_string());
    argv.push("--http.api".into());
    argv.push("eth,net,web3,txpool".into());
    if node.network != "dev" {
        argv.push("--syncmode".into());
        argv.push("snap".into());
        argv.push("--gcmode".into());
        argv.push(if node.mode == "full" {
            "archive".into()
        } else {
            "full".into()
        });
    }
    argv.extend(node.extra_flags.split_whitespace().map(str::to_string));
    argv
}

/// 构造 bitcoind 的 nexos-bitcoin.conf 内容（纯函数）。
///
/// **实测教训**（2026-09-03 bitcoind 31.1 冒烟，两轮）：
/// 1. 网络激活时 `port/rpcport/rpcbind/rpcallowip` 放顶层会被拒——
///    "Config setting for -port only applied on regtest network when in
///    [regtest] section"，进程直接退出；
/// 2. 反之 `regtest=1` 写在 `[regtest]` 节内**不会激活网络**（节内选项仅在
///    该网络已激活时生效）——bitcoind 会静默以 mainnet 启动连上主网！
///
/// 故正确形态：`<net>=1` 顶层激活 + 网络专属设置进 `[<net>]` 节（官方
/// bitcoin.conf 文档同款）。mainnet 无激活行、全顶层。fast → `prune=550`；
/// full+txindex → `txindex=1`（互斥由创建校验保证）。RPC 绑回环 + cookie
/// 认证（.cookie 落 datadir/<net>/）。
#[must_use]
pub fn build_bitcoin_conf(node: &ChainNode) -> String {
    let mut s = String::new();
    s.push_str("# NexOS blockchain_nodes 生成（节点重建会覆盖；自定义请用 extra_flags）\n");
    s.push_str("listen=1\n");
    let rpc_line = "rpcbind=127.0.0.1\nrpcallowip=127.0.0.1\n";
    let p2p_line = format!("port={}\n", node.p2p_port);
    let rpc_port_line = format!("rpcport={}\n", node.rpc_port);
    let mode_line = if node.mode == "fast" {
        "prune=550\n"
    } else if node.txindex {
        "txindex=1\n"
    } else {
        ""
    };
    match node.network.as_str() {
        "mainnet" => {
            s.push_str(rpc_line);
            s.push_str(&p2p_line);
            s.push_str(&rpc_port_line);
            s.push_str(mode_line);
        }
        net => {
            // 顶层激活 + 网络专属设置进节（顺序不可颠倒，见模块头实测教训）
            s.push_str(&format!("{net}=1\n"));
            s.push_str(&format!("[{net}]\n"));
            s.push_str(rpc_line);
            s.push_str(&p2p_line);
            s.push_str(&rpc_port_line);
            s.push_str(mode_line);
        }
    }
    s
}

/// 构造 bitcoind 启动 argv（conf 落 datadir/nexos-bitcoin.conf；前台运行由
/// 本模块作为子进程托管，不用 -daemon）。
#[must_use]
pub fn build_bitcoind_argv(node: &ChainNode) -> Vec<String> {
    let mut argv = vec![
        "bitcoind".to_string(),
        format!("-datadir={}", node.data_dir),
        format!("-conf={}/nexos-bitcoin.conf", node.data_dir),
    ];
    argv.extend(node.extra_flags.split_whitespace().map(str::to_string));
    argv
}

// ----------------------------------------------------------------------------
// ChainNodeState（实例表 + 进程表 + 落盘）
// ----------------------------------------------------------------------------

/// 运行进程登记（进程表；id → Child 句柄 + 日志路径 + argv）。
///
/// `child` 用 `Arc<tokio::sync::Mutex<Option<Child>>>`：监测任务 try_wait 收尸
/// （防僵尸——/proc/<pid> 对僵尸恒存在，不收尸状态修正会误判存活）、stop 语义
/// 为 SIGTERM → take + wait（10s 超时）→ SIGKILL + wait。kill_on_drop 未开：
/// os-api 退出不杀节点进程（重启后由状态修正收敛为 stopped，用户手动再启）。
struct NodeProcess {
    pid: u32,
    log_path: String,
    argv: Vec<String>,
    /// 启动时间（诊断口径；`started_at` 落 DB 行为 last_started_at）。
    #[allow(dead_code)]
    started_at: String,
    child: Arc<tokio::sync::Mutex<Option<tokio::process::Child>>>,
}

/// 子模块共享状态（BlockchainRouteHandler 持有并委托）。
pub struct ChainNodeState {
    /// SQLite（WAL，表 blockchain_nodes）。
    db: Arc<Mutex<Connection>>,
    /// 运行进程表（服务重启即清——pid 落库仅为展示，恢复时一律 stopped）。
    processes: Arc<Mutex<HashMap<String, Arc<NodeProcess>>>>,
    /// 日志目录。
    log_dir: String,
    /// 预估体积覆盖（env NEXOS_CHAIN_NODE_SIZE_HINTS 解析 + 测试注入）。
    size_overrides: Arc<Mutex<HashMap<String, u64>>>,
    /// 二进制路径覆盖（测试注入；生产走 env NEXOS_CHAIN_NODE_BIN_*）。
    bin_overrides: Arc<Mutex<HashMap<String, String>>>,
    /// id 计数器（从落盘行恢复最大值）。
    counter: Mutex<u64>,
}

impl ChainNodeState {
    /// 生产构造：DB 缺省 `/tank/os-data/chain-nodes.db`（/tank 缺失回退
    /// `./chain-nodes.db`），日志目录同理；env 均可覆盖。
    #[must_use]
    pub fn new() -> Self {
        let db_path = std::env::var(ENV_DB)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| {
                if Path::new("/tank/os-data").is_dir() {
                    "/tank/os-data/chain-nodes.db".to_string()
                } else {
                    "./chain-nodes.db".to_string()
                }
            });
        let log_dir = std::env::var(ENV_LOG_DIR)
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| {
                if Path::new("/tank/os-data").is_dir() {
                    "/tank/os-data/chain-node-logs".to_string()
                } else {
                    "./chain-node-logs".to_string()
                }
            });
        Self::from_db_path(&db_path, &log_dir)
    }

    /// 指定 DB 路径构造（部署/测试注入；体积覆盖解析 env）。
    #[must_use]
    pub fn from_db_path(db_path: &str, log_dir: &str) -> Self {
        if let Some(parent) = Path::new(db_path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::create_dir_all(log_dir);
        let conn = Connection::open(db_path)
            .or_else(|_| Connection::open_in_memory())
            .expect("打开 chain-nodes 数据库失败");
        let _ = conn.pragma_update(None, "journal_mode", "WAL");
        create_schema(&conn).expect("建 blockchain_nodes 表失败");
        let max_seq = load_rows_raw(&conn)
            .iter()
            .filter_map(|r| r.id.strip_prefix("cn-").and_then(|s| s.parse::<u64>().ok()))
            .max()
            .unwrap_or(0);
        Self {
            db: Arc::new(Mutex::new(conn)),
            processes: Arc::new(Mutex::new(HashMap::new())),
            log_dir: log_dir.to_string(),
            size_overrides: Arc::new(Mutex::new(parse_size_hints_str(&env_or_empty(
                ENV_SIZE_HINTS,
            )))),
            bin_overrides: Arc::new(Mutex::new(HashMap::new())),
            counter: Mutex::new(max_seq),
        }
    }

    /// 全内存构造（blockchain.rs with_empty 测试路径，不落盘文件）。
    #[must_use]
    pub fn in_memory() -> Self {
        let conn = Connection::open_in_memory().expect("内存库失败");
        create_schema(&conn).expect("建表失败");
        Self {
            db: Arc::new(Mutex::new(conn)),
            processes: Arc::new(Mutex::new(HashMap::new())),
            log_dir: std::env::temp_dir()
                .join("nexos-chain-node-logs")
                .to_string_lossy()
                .into_owned(),
            size_overrides: Arc::new(Mutex::new(parse_size_hints_str(&env_or_empty(
                ENV_SIZE_HINTS,
            )))),
            bin_overrides: Arc::new(Mutex::new(HashMap::new())),
            counter: Mutex::new(0),
        }
    }

    /// 体积覆盖注入（测试）。
    #[cfg(test)]
    pub fn set_size_override(&self, key: &str, gb: u64) {
        self.size_overrides
            .lock()
            .expect("size_overrides poisoned")
            .insert(key.to_string(), gb);
    }

    /// 二进制路径覆盖注入（测试）。
    #[cfg(test)]
    pub fn set_bin_override(&self, client: &str, path: &str) {
        self.bin_overrides
            .lock()
            .expect("bin_overrides poisoned")
            .insert(client.to_string(), path.to_string());
    }

    fn next_id(&self) -> String {
        let mut c = self.counter.lock().expect("counter poisoned");
        *c += 1;
        format!("cn-{c}")
    }

    fn log_path_for(&self, id: &str) -> String {
        let safe = id.replace(['/', ' ', '.'], "-");
        format!("{}/{}.log", self.log_dir.trim_end_matches('/'), safe)
    }

    /// 全量行快照（DB 直读，保留落库 status/pid——运行态修正由 fix_status 做）。
    fn rows(&self) -> Vec<ChainNode> {
        let conn = self.db.lock().expect("db poisoned");
        load_rows_raw(&conn)
    }

    /// 单行读取。
    fn row_by_id(&self, id: &str) -> Option<ChainNode> {
        self.rows().into_iter().find(|r| r.id == id)
    }

    /// 行落库（INSERT OR REPLACE 全量覆盖）。
    fn persist(&self, node: &ChainNode) {
        let conn = self.db.lock().expect("db poisoned");
        if let Err(e) = persist_row(&conn, node) {
            eprintln!("[blockchain_nodes] 落库失败 id={}: {e}", node.id);
        }
    }

    fn delete_row(&self, id: &str) {
        let conn = self.db.lock().expect("db poisoned");
        let _ = conn.execute("DELETE FROM blockchain_nodes WHERE id=?", params![id]);
    }

    /// 进程状态修正（列表/详情读取前调用）：status=running/syncing 且
    /// （无进程登记 或 Child 已退出且被收尸）→ stopped + error 记录
    /// （llm 实例同款语义；监测窗口外的退出靠这里兜底）。
    fn fix_status(&self, node: &mut ChainNode) -> bool {
        if node.status != "running" && node.status != "syncing" {
            return false;
        }
        let alive = self
            .processes
            .lock()
            .expect("processes poisoned")
            .get(&node.id)
            .map(|p| p.pid == node.pid.unwrap_or(0) && proc_alive(p.pid))
            .unwrap_or(false);
        if alive {
            return false;
        }
        node.status = "stopped".into();
        node.error = Some("进程已退出（服务重启或异常终止），请查看日志".into());
        node.pid = None;
        self.persist(node);
        true
    }

    /// 当前二进制解析快照。
    fn resolve_bin(&self, client: &str) -> Option<String> {
        let overrides = self
            .bin_overrides
            .lock()
            .expect("bin_overrides poisoned")
            .clone();
        resolve_client_bin(client, &overrides)
    }

    fn size_overrides_snapshot(&self) -> HashMap<String, u64> {
        self.size_overrides
            .lock()
            .expect("size_overrides poisoned")
            .clone()
    }

    /// 启动节点（核心路径：幂等检查 → 二进制检查 → 空间预检 → spawn → 监测）。
    async fn start_node(&self, id: &str) -> Result<serde_json::Value, String> {
        let mut node = self
            .row_by_id(id)
            .ok_or_else(|| format!("节点不存在: {id}"))?;
        // 0) 已在运行（进程表 pid 活着）→ 幂等返回
        if let Some(p) = self.processes.lock().expect("processes poisoned").get(id) {
            if proc_alive(p.pid) {
                return Ok(serde_json::json!({
                    "ok": true, "id": id, "status": node.status, "pid": p.pid,
                    "note": "节点已在运行", "command": p.argv.join(" "),
                    "log_path": p.log_path,
                    "rpc_url": format!("http://127.0.0.1:{}", node.rpc_port),
                }));
            }
        }
        // 1) 二进制探测（缺失 → 安装指引，caller 映射 409）
        let bin = self.resolve_bin(&node.client).ok_or_else(|| {
            format!(
                "{} 未安装（PATH 与常规路径均未命中）。{}",
                node.client,
                if node.client == "geth" {
                    geth_install_hint()
                } else {
                    bitcoind_install_hint()
                }
            )
        })?;
        // 2) 空间预检：full 阻断（caller 映射 409），fast 仅警示
        let overrides = self.size_overrides_snapshot();
        let sc = {
            let (k, n, m, d, t) = (
                node.kind.clone(),
                node.network.clone(),
                node.mode.clone(),
                node.data_dir.clone(),
                node.txindex,
            );
            tokio::task::spawn_blocking(move || space_check_blocking(&overrides, &k, &n, &m, &d, t))
                .await
                .map_err(|e| format!("空间预检任务失败: {e}"))?
        };
        if let Some(e) = &sc.error {
            return Err(format!("{e}（无法完成空间预检，禁止启动）"));
        }
        let mut warning = None;
        if !sc.sufficient {
            let msg = format!(
                "空间不足：{} {}/{} 模式预估需要 {}，{} 所在文件系统仅可用 {}",
                node.kind,
                node.network,
                node.mode,
                human_gb(sc.required_bytes),
                node.data_dir,
                human_bytes(sc.available_bytes),
            );
            if sc.blocking {
                return Err(format!("{msg}（全节点模式空间不足，禁止启动）"));
            }
            warning = Some(format!("{msg}（快速模式不阻断，仅供知悉）"));
        }
        // 3) 构造命令（bitcoind 先落 conf；geth 直 argv）
        let argv = if node.client == "geth" {
            build_geth_argv(&node)
        } else {
            let conf = build_bitcoin_conf(&node);
            std::fs::create_dir_all(&node.data_dir)
                .map_err(|e| format!("创建数据目录失败: {e}"))?;
            let conf_path = format!("{}/nexos-bitcoin.conf", node.data_dir);
            std::fs::write(&conf_path, conf).map_err(|e| format!("写入 {conf_path} 失败: {e}"))?;
            build_bitcoind_argv(&node)
        };
        let log_path = self.log_path_for(id);
        // 日志文件追加一行启动标记（排障锚点）
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
            use std::io::Write as _;
            let _ = writeln!(
                f,
                "===== nexos start {} client={} network={} mode={} =====",
                now_iso(),
                node.client,
                node.network,
                node.mode
            );
        }
        // 4) spawn（stdout/stderr 追加重定向到同一日志文件）
        let mut cmd = tokio::process::Command::new(&bin);
        cmd.args(&argv[1..]).stdin(Stdio::null());
        let stdout_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .ok();
        match stdout_file {
            Some(f) => {
                let ferr = f
                    .try_clone()
                    .map_err(|e| format!("日志文件 try_clone 失败: {e}"))?;
                cmd.stdout(Stdio::from(f)).stderr(Stdio::from(ferr));
            }
            None => {
                cmd.stdout(Stdio::null()).stderr(Stdio::null());
            }
        }
        let child = cmd.spawn().map_err(|e| format!("spawn {bin} 失败: {e}"))?;
        let pid = child
            .id()
            .ok_or_else(|| "spawn 成功但未取到 pid".to_string())?;
        let started_at = now_iso();
        // 5) 登记进程表 + 回写状态
        let needs_sync = matches!(
            (node.kind.as_str(), node.network.as_str()),
            ("ethereum", "mainnet")
                | ("ethereum", "sepolia")
                | ("bitcoin", "mainnet")
                | ("bitcoin", "testnet")
        );
        node.status = if needs_sync {
            "syncing".into()
        } else {
            "running".into()
        };
        node.pid = Some(pid);
        node.error = None;
        node.updated_at = now_iso();
        node.last_started_at = Some(started_at.clone());
        node.last_command = Some(format!("{} {}", bin, argv[1..].join(" ")));
        let entry = Arc::new(NodeProcess {
            pid,
            log_path: log_path.clone(),
            argv: argv.clone(),
            started_at,
            child: Arc::new(tokio::sync::Mutex::new(Some(child))),
        });
        self.processes
            .lock()
            .expect("processes poisoned")
            .insert(id.to_string(), Arc::clone(&entry));
        self.persist(&node);
        // 6) 后台监测（30s：Child 退出收尸 → error + 日志尾部；RPC 探活修正
        //    syncing→running；监测窗口外退出靠读取时状态修正兜底）
        spawn_monitor(Arc::clone(&self.db), self.processes.clone(), node.clone(), entry);
        let mut resp = serde_json::json!({
            "ok": true, "id": id, "status": node.status, "pid": pid,
            "binary": bin, "command": node.last_command,
            "log_path": log_path,
            "rpc_url": format!("http://127.0.0.1:{}", node.rpc_port),
            "space": serde_json::to_value(&sc).unwrap_or(serde_json::Value::Null),
        });
        if let Some(w) = warning {
            resp["warning"] = serde_json::Value::String(w);
        }
        Ok(resp)
    }

    /// 停止节点：SIGTERM → take Child + wait（10s 超时）→ SIGKILL + wait。
    async fn stop_node(&self, id: &str) -> Result<serde_json::Value, String> {
        let mut node = self
            .row_by_id(id)
            .ok_or_else(|| format!("节点不存在: {id}"))?;
        let entry = self
            .processes
            .lock()
            .expect("processes poisoned")
            .get(id)
            .cloned();
        let mut note = "无运行进程（已停止）".to_string();
        if let Some(p) = entry {
            if proc_alive(p.pid) {
                let _ = Command::new("kill").arg(p.pid.to_string()).spawn();
            }
            // take Child → wait 收尸（退出后 /proc 消失，状态修正不误判）
            let mut guard = p.child.lock().await;
            if let Some(mut child) = guard.take() {
                match tokio::time::timeout(Duration::from_secs(STOP_GRACE_SECS), child.wait())
                    .await
                {
                    Ok(_) => note = format!("已停止（pid={}，SIGTERM 优雅退出）", p.pid),
                    Err(_) => {
                        // SIGTERM 超时 → SIGKILL + wait
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        note = format!("SIGTERM {}s 未退出，已 SIGKILL（pid={}）", STOP_GRACE_SECS, p.pid);
                    }
                }
            }
            self.processes.lock().expect("processes poisoned").remove(id);
        }
        node.status = "stopped".into();
        node.pid = None;
        node.updated_at = now_iso();
        self.persist(&node);
        Ok(serde_json::json!({
            "ok": true, "id": id, "status": "stopped", "note": note,
            "log_path": self.log_path_for(id),
        }))
    }
}

impl Default for ChainNodeState {
    fn default() -> Self {
        Self::new()
    }
}

/// spawn 后监测任务：SPAWN_MONITOR_POLLS × 5s 内——
/// 1. Child try_wait 退出 → 收尸 + 回写 error（附日志尾部）+ 移除进程表登记；
/// 2. 节点 syncing 且 RPC 探活成功（geth eth_syncing=false / bitcoind RPC 端口
///    可连）→ 回写 running。
///
/// 监测窗口（30s）外退出：进程表登记仍在，读取时 [`ChainNodeState::fix_status`]
/// 经 /proc 探测兜底修正（窗口后不再收尸——僵尸由下次同 id start/stop 或
/// 服务重启清理，可接受：写路径固定走 take+wait）。
fn spawn_monitor(
    db: Arc<Mutex<Connection>>,
    processes: Arc<Mutex<HashMap<String, Arc<NodeProcess>>>>,
    mut node: ChainNode,
    entry: Arc<NodeProcess>,
) {
    tokio::spawn(async move {
        for _ in 0..SPAWN_MONITOR_POLLS {
            tokio::time::sleep(SPAWN_MONITOR_INTERVAL).await;
            let exited = {
                let mut guard = entry.child.lock().await;
                match guard.as_mut().map(tokio::process::Child::try_wait) {
                    Some(Ok(Some(status))) => Some(status),
                    Some(Err(_)) | None => None, // Err/句柄被 stop take走 → 视为非本任务管辖
                    Some(Ok(None)) => None,
                }
            };
            if let Some(status) = exited {
                // 确认非有意停止（stop 会先移除进程表登记）
                let intentional = {
                    let map = processes.lock().expect("processes poisoned");
                    !map.contains_key(&node.id)
                };
                if !intentional {
                    let tail = read_log_tail(&entry.log_path, 8).join("\n");
                    node.status = "error".into();
                    node.error = Some(format!(
                        "进程已退出（code={}）。日志尾部:\n{tail}",
                        status.code().map_or_else(|| "signal".to_string(), |c| c.to_string())
                    ));
                    node.pid = None;
                    node.updated_at = now_iso();
                    if let Ok(conn) = db.lock() {
                        let _ = persist_row(&conn, &node);
                    }
                    eprintln!("[blockchain_nodes] 节点 {} 进程退出", node.id);
                }
                return;
            }
            if node.status == "syncing" && rpc_probe_ready(&node).await {
                node.status = "running".into();
                node.updated_at = now_iso();
                if let Ok(conn) = db.lock() {
                    let _ = persist_row(&conn, &node);
                }
                return;
            }
        }
    });
}

/// pid 存活探测（/proc/<pid>，monitor.rs 同款口径；linux-only）。
pub(crate) fn proc_alive(pid: u32) -> bool {
    std::fs::metadata(format!("/proc/{pid}")).is_ok()
}

/// RPC 探活：geth → POST eth_syncing（result=false 即追平）；bitcoind → TCP 连
/// rpc 端口成功即视为就绪（bitcoind 启动期间 RPC 不可达）。
async fn rpc_probe_ready(node: &ChainNode) -> bool {
    if node.client == "geth" {
        let url = format!("http://127.0.0.1:{}/", node.rpc_port);
        let Ok(resp) = HTTP
            .post(&url)
            .json(&serde_json::json!({
                "jsonrpc": "2.0", "method": "eth_syncing", "params": [], "id": 1
            }))
            .send()
            .await
        else {
            return false;
        };
        let Ok(v) = resp.json::<serde_json::Value>().await else {
            return false;
        };
        v.get("result") == Some(&serde_json::Value::Bool(false))
    } else {
        tokio::net::TcpStream::connect(("127.0.0.1", node.rpc_port))
            .await
            .is_ok()
    }
}

/// 读日志尾部 N 行（文件缺失 → 空 vec）。
pub(crate) fn read_log_tail(path: &str, tail: usize) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return vec![];
    };
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(tail);
    lines[start..].iter().map(|s| (*s).to_string()).collect()
}

/// 人读 GB（GiB 口径）。
fn human_gb(bytes: u64) -> String {
    format!("{}GB", bytes / (1024 * 1024 * 1024))
}

/// 人读字节（<1GB 显示 MB/KB）。
fn human_bytes(bytes: u64) -> String {
    const GB: u64 = 1024 * 1024 * 1024;
    const MB: u64 = 1024 * 1024;
    if bytes >= GB {
        format!("{:.1}GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.0}MB", bytes as f64 / MB as f64)
    } else {
        format!("{bytes}B")
    }
}

fn now_iso() -> String {
    use chrono::Local;
    Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

/// 读非空 env（空串视为未设置）。
fn env_or_empty(key: &str) -> String {
    std::env::var(key).unwrap_or_default()
}

/// 解析体积覆盖 JSON（object str→u64；非法整体忽略 → 空 map）。
fn parse_size_hints_str(raw: &str) -> HashMap<String, u64> {
    if raw.trim().is_empty() {
        return HashMap::new();
    }
    serde_json::from_str::<HashMap<String, u64>>(raw).unwrap_or_default()
}

// ----------------------------------------------------------------------------
// SQLite（表/行映射；blockchain_nodes 表）
// ----------------------------------------------------------------------------

fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS blockchain_nodes (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            kind TEXT NOT NULL,
            network TEXT NOT NULL,
            client TEXT NOT NULL,
            mode TEXT NOT NULL DEFAULT 'fast',
            data_dir TEXT NOT NULL,
            rpc_port INTEGER NOT NULL,
            p2p_port INTEGER NOT NULL,
            txindex INTEGER NOT NULL DEFAULT 0,
            extra_flags TEXT NOT NULL DEFAULT '',
            status TEXT NOT NULL DEFAULT 'stopped',
            pid INTEGER,
            error TEXT,
            created_at TEXT,
            updated_at TEXT,
            last_started_at TEXT,
            last_command TEXT
        );",
    )?;
    // 迁移：早期表缺列（CREATE IF NOT EXISTS 不补列；已存在则 ALTER 报
    // duplicate column，忽略即幂等——llm_instances 同款惯例）
    let _ = conn.execute("ALTER TABLE blockchain_nodes ADD COLUMN updated_at TEXT", []);
    let _ = conn.execute(
        "ALTER TABLE blockchain_nodes ADD COLUMN last_started_at TEXT",
        [],
    );
    let _ = conn.execute("ALTER TABLE blockchain_nodes ADD COLUMN last_command TEXT", []);
    Ok(())
}

fn persist_row(conn: &Connection, n: &ChainNode) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO blockchain_nodes
         (id,name,kind,network,client,mode,data_dir,rpc_port,p2p_port,txindex,
          extra_flags,status,pid,error,created_at,updated_at,last_started_at,last_command)
         VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![
            n.id,
            n.name,
            n.kind,
            n.network,
            n.client,
            n.mode,
            n.data_dir,
            i64::from(n.rpc_port),
            i64::from(n.p2p_port),
            i64::from(n.txindex),
            n.extra_flags,
            n.status,
            n.pid.map(i64::from),
            n.error.as_deref(),
            n.created_at,
            n.updated_at,
            n.last_started_at.as_deref(),
            n.last_command.as_deref(),
        ],
    )?;
    Ok(())
}

/// 读全部行（原样保留落库 status/pid/error——服务运行期 `rows()` 用；
/// 状态修正语义见 [`ChainNodeState::fix_status`]）。
fn load_rows_raw(conn: &Connection) -> Vec<ChainNode> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT id,name,kind,network,client,mode,data_dir,rpc_port,p2p_port,txindex,
                extra_flags,status,pid,error,created_at,updated_at,last_started_at,last_command
         FROM blockchain_nodes ORDER BY created_at, id",
    ) else {
        return vec![];
    };
    let Ok(iter) = stmt.query_map([], |row| {
        Ok(ChainNode {
            id: row.get(0)?,
            name: row.get(1)?,
            kind: row.get(2)?,
            network: row.get(3)?,
            client: row.get(4)?,
            mode: row.get(5)?,
            data_dir: row.get(6)?,
            rpc_port: u16::try_from(row.get::<_, i64>(7)?).unwrap_or(0),
            p2p_port: u16::try_from(row.get::<_, i64>(8)?).unwrap_or(0),
            txindex: row.get::<_, i64>(9)? == 1,
            extra_flags: row.get(10)?,
            status: row.get(11)?,
            pid: row.get::<_, Option<i64>>(12)?.and_then(|p| u32::try_from(p).ok()),
            error: row.get(13)?,
            created_at: row.get(14)?,
            updated_at: row.get::<_, Option<String>>(15)?.unwrap_or_default(),
            last_started_at: row.get(16)?,
            last_command: row.get(17)?,
        })
    }) else {
        return vec![];
    };
    iter.filter_map(Result::ok).collect()
}

// 服务重启恢复语义说明：DB 行的 status/pid 原样读取（`load_rows_raw`），但
// 进程表重启即空——首次读取时 [`ChainNodeState::fix_status`] 会把 running/
// syncing 行修正为 stopped + "进程已退出（服务重启或异常终止）"（等价于
// llm_instances 的恢复期强制 stopped；不自动恢复运行态，用户经 UI 重新启动）。

// ----------------------------------------------------------------------------
// 路由 specs + handle（BlockchainRouteHandler 委托，前缀 /api/v1/blockchain/chain-nodes）
// ----------------------------------------------------------------------------

/// 路由 specs（handler_component=blockchain，与父 handler 现有端点同风格）。
pub fn route_specs() -> Vec<RouteSpec> {
    fn spec(method: HttpMethod, path: &str, requires_auth: bool, roles: Vec<String>) -> RouteSpec {
        RouteSpec {
            method,
            path: path.to_string(),
            handler_component: "blockchain".to_string(),
            requires_auth,
            required_roles: roles,
        }
    }
    vec![
        spec(HttpMethod::Get, "/api/v1/blockchain/chain-nodes", false, vec![]),
        spec(HttpMethod::Post, "/api/v1/blockchain/chain-nodes", true, vec!["admin".into()]),
        spec(HttpMethod::Get, "/api/v1/blockchain/chain-nodes/presets", false, vec![]),
        spec(HttpMethod::Get, "/api/v1/blockchain/chain-nodes/space-check", false, vec![]),
        spec(HttpMethod::Get, "/api/v1/blockchain/chain-nodes/:id", false, vec![]),
        spec(
            HttpMethod::Post,
            "/api/v1/blockchain/chain-nodes/:id/start",
            true,
            vec!["admin".into()],
        ),
        spec(
            HttpMethod::Post,
            "/api/v1/blockchain/chain-nodes/:id/stop",
            true,
            vec!["admin".into()],
        ),
        spec(
            HttpMethod::Delete,
            "/api/v1/blockchain/chain-nodes/:id",
            true,
            vec!["admin".into()],
        ),
        spec(HttpMethod::Get, "/api/v1/blockchain/chain-nodes/:id/logs", false, vec![]),
    ]
}

fn ok_json(body: serde_json::Value) -> ApiResponse {
    ApiResponse {
        status: 200,
        body,
        headers: serde_json::json!({}),
    }
}

fn created_json(body: serde_json::Value) -> ApiResponse {
    ApiResponse {
        status: 201,
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

/// 简易 query 解析（& 分隔 + %XX 解码；无依赖）。
fn parse_query(query: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        m.insert(percent_decode(k), percent_decode(v));
    }
    m
}

/// 百分号解码（+ → 空格；非法 %XX 原样保留）。
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = (b[i + 1] as char).to_digit(16).zip((b[i + 2] as char).to_digit(16));
                match hex {
                    Some((h, l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    None => {
                        out.push(b[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 校验 kind+network 组合（白名单）。
fn valid_network(kind: &str, network: &str) -> bool {
    matches!(
        (kind, network),
        ("ethereum", "mainnet")
            | ("ethereum", "sepolia")
            | ("ethereum", "dev")
            | ("bitcoin", "mainnet")
            | ("bitcoin", "testnet")
            | ("bitcoin", "regtest")
    )
}

/// 校验数据目录：绝对路径 + 无 `..` 穿越段 + 无 NUL。
fn valid_data_dir(dir: &str) -> bool {
    dir.starts_with('/') && !dir.split('/').any(|s| s == "..") && !dir.contains('\0')
}

/// handle 入口（blockchain.rs 对 `["api","v1","blockchain","chain-nodes",..]`
/// 前缀整体委托；segs 为前缀之后的段，query 为原始查询串）。
pub async fn handle(
    state: &ChainNodeState,
    method: HttpMethod,
    segs: &[&str],
    query: &str,
    body: serde_json::Value,
) -> Result<ApiResponse, ApiGatewayError> {
    // 静态段（presets / space-check）须在 :id 捕获之前匹配
    match (method, segs) {
        // —— GET /presets —— 链/网络/模式预设 + 预估体积 + 二进制探测
        (HttpMethod::Get, ["presets"]) => {
            let bin_overrides = self_bin_overrides(state);
            let mut presets = network_presets();
            for p in &mut presets {
                p.binary_installed =
                    resolve_client_bin(&p.default_client, &bin_overrides).is_some();
                p.install_hint = if p.default_client == "geth" {
                    geth_install_hint()
                } else {
                    bitcoind_install_hint()
                };
            }
            Ok(ok_json(serde_json::json!({
                "presets": presets,
                "default_data_root": default_data_root(),
                "size_hint_source": format!("内置调研表（env {ENV_SIZE_HINTS} 可覆盖）"),
            })))
        }

        // —— GET /space-check —— 空间预检（创建向导实时调用）
        (HttpMethod::Get, ["space-check"]) => {
            let q = parse_query(query);
            let kind = q.get("kind").cloned().unwrap_or_default();
            let network = q.get("network").cloned().unwrap_or_default();
            let mode = q.get("mode").cloned().unwrap_or_else(|| "fast".into());
            let data_dir = q.get("data_dir").cloned().unwrap_or_default();
            let txindex = q
                .get("txindex")
                .is_some_and(|v| v == "1" || v == "true");
            if !valid_network(&kind, &network) {
                return Ok(error_response(
                    400,
                    &format!("kind/network 非法: {kind}/{network}"),
                ));
            }
            if mode != "fast" && mode != "full" {
                return Ok(error_response(400, &format!("mode 非法（fast|full）: {mode}")));
            }
            if data_dir.is_empty() || !valid_data_dir(&data_dir) {
                return Ok(error_response(400, "data_dir 非法（须为绝对路径且不含 ..）"));
            }
            let overrides = state.size_overrides_snapshot();
            let sc = tokio::task::spawn_blocking(move || {
                space_check_blocking(&overrides, &kind, &network, &mode, &data_dir, txindex)
            })
            .await
            .map_err(|e| ApiGatewayError::Internal(format!("空间预检任务失败: {e}")))?;
            Ok(ok_json(
                serde_json::to_value(&sc).unwrap_or(serde_json::Value::Null),
            ))
        }

        // —— GET /（列表）—— 含状态修正
        (HttpMethod::Get, []) => {
            let mut rows = state.rows();
            for n in &mut rows {
                state.fix_status(n);
            }
            Ok(ok_json(
                serde_json::to_value(&rows).unwrap_or(serde_json::Value::Null),
            ))
        }

        // —— POST /（创建）—— admin；full 空间预检不过 → 409
        (HttpMethod::Post, []) => {
            let body: CreateNodeBody = serde_json::from_value(body).map_err(|e| {
                ApiGatewayError::Internal(format!("解析创建节点请求体失败: {e}"))
            })?;
            let name = body.name.trim().to_string();
            if name.is_empty() {
                return Ok(error_response(400, "name 不可为空"));
            }
            let (kind, network) = (body.kind.trim(), body.network.trim());
            if !valid_network(kind, network) {
                return Ok(error_response(
                    400,
                    &format!(
                        "kind/network 非法: {kind}/{network}\
                         （支持 eth mainnet/sepolia/dev + btc mainnet/testnet/regtest）"
                    ),
                ));
            }
            // mode：缺省 fast；显式给值必须是 fast|full（不静默矫正——手滑
            // 值（如 light/archive）直接 400，由 UI 预设约束正常输入）
            let mode = match body.mode.as_deref().map(str::trim) {
                None | Some("") => "fast".to_string(),
                Some("fast") | Some("full") => body.mode.clone().unwrap_or_default(),
                Some(m) => {
                    return Ok(error_response(400, &format!("mode 非法（fast|full）: {m}")))
                }
            };
            let client = body
                .client
                .filter(|c| !c.trim().is_empty())
                .unwrap_or_else(|| {
                    if kind == "ethereum" {
                        "geth".into()
                    } else {
                        "bitcoind".into()
                    }
                });
            let expected_client = if kind == "ethereum" { "geth" } else { "bitcoind" };
            if client != expected_client {
                return Ok(error_response(
                    400,
                    &format!("kind={kind} 仅支持 client={expected_client}"),
                ));
            }
            let txindex = body.txindex.unwrap_or(false);
            if txindex && !(kind == "bitcoin" && mode == "full") {
                return Ok(error_response(
                    400,
                    "txindex 仅 bitcoin full 模式可选（prune 与 txindex 互斥）",
                ));
            }
            let preset = network_presets()
                .into_iter()
                .find(|p| p.kind == kind && p.network == network)
                .unwrap_or_else(|| network_presets().remove(0));
            let data_dir = body
                .data_dir
                .map(|d| d.trim().to_string())
                .filter(|d| !d.is_empty())
                .unwrap_or_else(|| format!("{}/{}", default_data_root(), network));
            if !valid_data_dir(&data_dir) {
                return Ok(error_response(400, "data_dir 非法（须为绝对路径且不含 ..）"));
            }
            let rpc_port = body.rpc_port.unwrap_or(preset.default_rpc_port);
            let p2p_port = body.p2p_port.unwrap_or(preset.default_p2p_port);
            if !(1024..=65535).contains(&rpc_port) || !(1024..=65535).contains(&p2p_port) {
                return Ok(error_response(400, "端口须在 1024-65535"));
            }
            // 空间预检：full 阻断（409），fast 警示不阻断
            let overrides = state.size_overrides_snapshot();
            let sc = {
                let (k, n, m, d) = (
                    kind.to_string(),
                    network.to_string(),
                    mode.clone(),
                    data_dir.clone(),
                );
                tokio::task::spawn_blocking(move || {
                    space_check_blocking(&overrides, &k, &n, &m, &d, txindex)
                })
                .await
                .map_err(|e| ApiGatewayError::Internal(format!("空间预检任务失败: {e}")))?
            };
            if let Some(e) = &sc.error {
                return Ok(error_response(400, e));
            }
            let mut warnings: Vec<String> = vec![];
            if !sc.sufficient {
                let msg = format!(
                    "空间不足：需要 {}，{} 所在文件系统仅可用 {}",
                    human_gb(sc.required_bytes),
                    data_dir,
                    human_bytes(sc.available_bytes),
                );
                if sc.blocking {
                    return Ok(error_response(
                        409,
                        &format!(
                            "{msg}——全节点（full）模式空间不足，禁止创建；\
                             可改用快速（fast）模式或更换数据目录"
                        ),
                    ));
                }
                warnings.push(format!("{msg}（快速模式不阻断，仅供知悉）"));
            }
            if preset.requires_consensus_client {
                warnings.push(
                    "post-merge PoS 网络：geth 仅执行层（EL），需另配共识客户端（如 \
                     lighthouse）才能跟住链头——见 docs/BLOCKCHAIN_NODES.md §CL 搭配指引"
                        .into(),
                );
            }
            // 二进制探测（不阻断创建；start 时再硬检查）
            let bin = state.resolve_bin(&client);
            let now = now_iso();
            let id = state.next_id();
            let node = ChainNode {
                id: id.clone(),
                name,
                kind: kind.to_string(),
                network: network.to_string(),
                client: client.clone(),
                mode,
                data_dir,
                rpc_port,
                p2p_port,
                txindex,
                extra_flags: body.extra_flags.unwrap_or_default(),
                status: "stopped".into(),
                pid: None,
                error: None,
                created_at: now.clone(),
                updated_at: now,
                last_started_at: None,
                last_command: None,
            };
            state.persist(&node);
            let mut resp = serde_json::json!({
                "node": serde_json::to_value(&node).unwrap_or(serde_json::Value::Null),
                "space": serde_json::to_value(&sc).unwrap_or(serde_json::Value::Null),
                "binary_installed": bin.is_some(),
                "binary_path": bin,
                "install_hint": if client.as_str() == "geth" {
                    geth_install_hint()
                } else {
                    bitcoind_install_hint()
                },
                "rpc_url": format!("http://127.0.0.1:{rpc_port}"),
                "requires_consensus_client": preset.requires_consensus_client,
            });
            if !warnings.is_empty() {
                resp["warnings"] = serde_json::Value::Array(
                    warnings
                        .into_iter()
                        .map(serde_json::Value::String)
                        .collect(),
                );
            }
            Ok(created_json(resp))
        }

        // —— GET /:id —— 详情（含状态修正 + 二进制/日志路径快照）
        (HttpMethod::Get, [id]) => {
            let mut node = match state.row_by_id(id) {
                Some(n) => n,
                None => return Ok(error_response(404, &format!("节点不存在: {id}"))),
            };
            state.fix_status(&mut node);
            let bin = state.resolve_bin(&node.client);
            Ok(ok_json(serde_json::json!({
                "node": serde_json::to_value(&node).unwrap_or(serde_json::Value::Null),
                "binary_installed": bin.is_some(),
                "binary_path": bin,
                "rpc_url": format!("http://127.0.0.1:{}", node.rpc_port),
                "log_path": state.log_path_for(id),
            })))
        }

        // —— POST /:id/start —— 启动（admin；409=二进制缺失/空间不足，502=spawn 失败）
        (HttpMethod::Post, [id, "start"]) => match state.start_node(id).await {
            Ok(v) => Ok(ok_json(v)),
            Err(msg) => {
                let status = if msg.starts_with("节点不存在") {
                    404
                } else if msg.contains("未安装") || msg.contains("空间不足") || msg.contains("空间预检") {
                    409
                } else {
                    502
                };
                if status == 502 {
                    if let Some(mut node) = state.row_by_id(id) {
                        node.status = "error".into();
                        node.error = Some(msg.clone());
                        node.updated_at = now_iso();
                        state.persist(&node);
                    }
                }
                Ok(error_response(status, &msg))
            }
        },

        // —— POST /:id/stop —— 停止（admin）
        (HttpMethod::Post, [id, "stop"]) => match state.stop_node(id).await {
            Ok(v) => Ok(ok_json(v)),
            Err(msg) => Ok(error_response(
                if msg.starts_with("节点不存在") {
                    404
                } else {
                    502
                },
                &msg,
            )),
        },

        // —— DELETE /:id —— 删除（admin；先尽力停止；链数据目录不自动删）
        (HttpMethod::Delete, [id]) => {
            let node = match state.row_by_id(id) {
                Some(n) => n,
                None => return Ok(error_response(404, &format!("节点不存在: {id}"))),
            };
            let _ = state.stop_node(id).await;
            state.delete_row(id);
            Ok(ok_json(serde_json::json!({
                "ok": true, "id": id, "action": "delete",
                "data_dir_kept": true,
                "note": format!(
                    "链数据目录 {} 未删除（同步产物动辄数百 GB，请确认后手动清理）",
                    node.data_dir
                ),
            })))
        }

        // —— GET /:id/logs —— 日志尾部（?tail=200）
        (HttpMethod::Get, [id, "logs"]) => {
            let mut node = match state.row_by_id(id) {
                Some(n) => n,
                None => return Ok(error_response(404, &format!("节点不存在: {id}"))),
            };
            state.fix_status(&mut node);
            let q = parse_query(query);
            let tail = q
                .get("tail")
                .and_then(|t| t.parse::<usize>().ok())
                .map(|t| t.clamp(1, LOG_TAIL_MAX))
                .unwrap_or(LOG_TAIL_DEFAULT);
            let path = state.log_path_for(id);
            let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let lines = read_log_tail(&path, tail);
            Ok(ok_json(serde_json::json!({
                "id": id, "status": node.status,
                "log_path": path, "size_bytes": size,
                "returned_lines": lines.len(), "tail": tail,
                "lines": lines,
            })))
        }

        _ => Ok(error_response(
            404,
            &format!(
                "未匹配路由: /api/v1/blockchain/chain-nodes/{}",
                segs.join("/")
            ),
        )),
    }
}

/// 测试注入与 env 的二进制覆盖快照（presets 端点用）。
fn self_bin_overrides(state: &ChainNodeState) -> HashMap<String, String> {
    state
        .bin_overrides
        .lock()
        .expect("bin_overrides poisoned")
        .clone()
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试临时目录守卫（Drop 清理）。
    struct TempDirGuard {
        path: String,
    }

    impl Drop for TempDirGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn tempdir(tag: &str) -> TempDirGuard {
        let p = std::env::temp_dir().join(format!("nexos-bcn-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("建临时目录失败");
        TempDirGuard {
            path: p.to_string_lossy().into_owned(),
        }
    }

    fn state_in_tmp(tag: &str) -> (ChainNodeState, TempDirGuard) {
        let dir = tempdir(tag);
        let s = ChainNodeState::from_db_path(
            &format!("{}/chain-nodes.db", dir.path),
            &format!("{}/logs", dir.path),
        );
        (s, dir)
    }

    async fn h(
        state: &ChainNodeState,
        method: HttpMethod,
        segs: &[&str],
        query: &str,
        body: serde_json::Value,
    ) -> ApiResponse {
        handle(state, method, segs, query, body)
            .await
            .expect("handle 不应失败")
    }

    async fn space_check_q(state: &ChainNodeState, query: &str) -> ApiResponse {
        h(state, HttpMethod::Get, &["space-check"], query, serde_json::Value::Null).await
    }

    // ---- 预设与体积表 ----

    #[test]
    fn builtin_size_hints_cover_all_preset_modes() {
        let hints = builtin_size_hints();
        for p in network_presets() {
            for m in &p.modes {
                let key = format!("{}/{}/{}", p.kind, p.network, m.mode);
                assert!(hints.contains_key(&key), "缺预估体积: {key}");
                assert!(m.estimated_size_gb > 0, "{key} 体积应为正");
            }
        }
    }

    #[test]
    fn default_data_root_resolves_writable_base() {
        let root = default_data_root();
        assert!(!root.is_empty());
        assert!(root.starts_with('/') || root.starts_with('.'), "root: {root}");
        // 解析出的 base 应可写（在本测试进程权限下创建探针文件）
        assert!(
            dir_writable(&root) || std::fs::create_dir_all(&root).is_ok(),
            "数据根目录应可创建/可写: {root}"
        );
    }

    #[test]
    fn resolve_size_gb_override_wins() {
        let mut ov = HashMap::new();
        ov.insert("ethereum/mainnet/fast".to_string(), 42);
        assert_eq!(resolve_size_gb(&ov, "ethereum", "mainnet", "fast"), 42);
        assert_eq!(resolve_size_gb(&ov, "ethereum", "mainnet", "full"), 2200);
        assert_eq!(
            resolve_size_gb(&ov, "unknown", "net", "fast"),
            1,
            "未知键 1GB 兜底"
        );
    }

    #[test]
    fn parse_size_hints_str_accepts_json_rejects_garbage() {
        let m = parse_size_hints_str(r#"{"ethereum/mainnet/fast": 650}"#);
        assert_eq!(m.get("ethereum/mainnet/fast"), Some(&650));
        assert!(parse_size_hints_str("not json").is_empty());
        assert!(parse_size_hints_str("").is_empty());
    }

    #[test]
    fn presets_have_honest_consensus_and_prune_notes() {
        let ps = network_presets();
        let mainnet = ps
            .iter()
            .find(|p| p.kind == "ethereum" && p.network == "mainnet")
            .unwrap();
        assert!(mainnet.requires_consensus_client, "ETH 主网需 CL");
        assert!(mainnet
            .modes
            .iter()
            .any(|m| m.note.as_deref().unwrap_or("").contains("LES")));
        let btc = ps
            .iter()
            .find(|p| p.kind == "bitcoin" && p.network == "mainnet")
            .unwrap();
        assert!(
            btc.modes
                .iter()
                .any(|m| m.mode == "fast" && m.flags.contains("prune=550")),
            "BTC fast 应为 prune=550"
        );
    }

    // ---- 命令构造纯函数 ----

    fn sample_node(kind: &str, network: &str, mode: &str) -> ChainNode {
        ChainNode {
            id: "cn-1".into(),
            name: "t".into(),
            kind: kind.into(),
            network: network.into(),
            client: if kind == "ethereum" {
                "geth".into()
            } else {
                "bitcoind".into()
            },
            mode: mode.into(),
            data_dir: "/tank/blockchain/x".into(),
            rpc_port: 8545,
            p2p_port: 30303,
            txindex: false,
            extra_flags: String::new(),
            status: "stopped".into(),
            pid: None,
            error: None,
            created_at: "t".into(),
            updated_at: "t".into(),
            last_started_at: None,
            last_command: None,
        }
    }

    #[test]
    fn geth_argv_fast_vs_full_vs_networks() {
        let fast = build_geth_argv(&sample_node("ethereum", "mainnet", "fast"));
        let joined = fast.join(" ");
        assert!(joined.contains("--syncmode snap"), "fast 应 snap: {joined}");
        assert!(joined.contains("--gcmode full"), "fast 应 gcmode full: {joined}");
        assert!(!joined.contains("--mainnet"), "geth 主网无网络旗标: {joined}");
        assert!(joined.contains("--http.addr 127.0.0.1"), "RPC 绑回环: {joined}");

        let full = build_geth_argv(&sample_node("ethereum", "mainnet", "full"));
        assert!(full.join(" ").contains("--gcmode archive"), "full 应 archive");

        let sep = build_geth_argv(&sample_node("ethereum", "sepolia", "fast"));
        assert!(sep.join(" ").contains("--sepolia"));

        let dev = build_geth_argv(&sample_node("ethereum", "dev", "fast"));
        let dj = dev.join(" ");
        assert!(dj.contains("--dev"), "dev 链用 --dev: {dj}");
        assert!(!dj.contains("--gcmode"), "dev 不带 gcmode: {dj}");
    }

    #[test]
    fn geth_argv_extra_flags_appended() {
        let mut n = sample_node("ethereum", "sepolia", "fast");
        n.extra_flags = "--http.api eth,net --verbosity 4".into();
        let argv = build_geth_argv(&n);
        assert!(argv.contains(&"--verbosity".to_string()));
        assert!(argv.last().is_some_and(|s| s == "4"), "extra_flags 追加在尾部");
    }

    #[test]
    fn bitcoin_conf_prune_vs_txindex_vs_networks() {
        let fast = build_bitcoin_conf(&sample_node("bitcoin", "mainnet", "fast"));
        assert!(fast.contains("prune=550"));
        assert!(!fast.contains("txindex"));
        assert!(!fast.contains("testnet="), "主网无网络节");
        assert!(!fast.contains('['), "主网全部顶层: {fast}");

        let mut full = sample_node("bitcoin", "mainnet", "full");
        full.txindex = true;
        let conf = build_bitcoin_conf(&full);
        assert!(conf.contains("txindex=1"));
        assert!(!conf.contains("prune"), "full 无 prune");

        let full_noindex = build_bitcoin_conf(&sample_node("bitcoin", "mainnet", "full"));
        assert!(!full_noindex.contains("txindex") && !full_noindex.contains("prune"));

        // 非 mainnet：顶层激活 + 网络专属设置进 [<net>] 节（实测教训：激活行
        // 写节内会静默以 mainnet 启动；端口行放顶层会被拒——见 build_bitcoin_conf）
        let reg = build_bitcoin_conf(&sample_node("bitcoin", "regtest", "fast"));
        assert!(
            reg.contains("\nregtest=1\n") && reg.find("regtest=1").unwrap() < reg.find("[regtest]\n").unwrap(),
            "regtest=1 应在节头之前顶层激活: {reg}"
        );
        assert!(reg.contains("[regtest]\n"), "regtest 应有节头: {reg}");
        let reg_body = reg.split("[regtest]\n").nth(1).unwrap_or("");
        assert!(reg_body.contains("rpcport=8545") && reg_body.contains("port=30303"));
        assert!(reg.contains("prune=550"));

        let tn = build_bitcoin_conf(&sample_node("bitcoin", "testnet", "fast"));
        assert!(tn.contains("\ntestnet=1\n") && tn.contains("[testnet]\n"));
        assert!(
            tn.split("[testnet]\n").nth(1).unwrap_or("").contains("prune=550"),
            "prune 应在 testnet 节内: {tn}"
        );
    }

    #[test]
    fn bitcoind_argv_uses_dedicated_conf() {
        let argv = build_bitcoind_argv(&sample_node("bitcoin", "mainnet", "fast"));
        let j = argv.join(" ");
        assert!(j.contains("-datadir=/tank/blockchain/x"));
        assert!(j.contains("-conf=/tank/blockchain/x/nexos-bitcoin.conf"));
        assert!(!j.contains("-daemon"), "前台运行由模块托管");
    }

    // ---- 空间预检 ----

    #[test]
    fn df_bytes_parses_and_degrades() {
        let some = df_bytes("/");
        assert!(some.is_some(), "df / 应可解析");
        assert!(some.as_ref().unwrap().1 > 0);
        assert!(
            df_bytes("/nonexistent-abcxyz").is_none(),
            "不存在路径 → None"
        );
    }

    #[test]
    fn space_check_full_insufficient_flags_blocking() {
        let dir = tempdir("sc");
        let mut ov = HashMap::new();
        ov.insert("ethereum/mainnet/full".to_string(), 1_000_000);
        let sc = space_check_blocking(
            &ov,
            "ethereum",
            "mainnet",
            "full",
            &format!("{}/d", dir.path),
            false,
        );
        assert!(sc.blocking, "full 模式阻断");
        assert!(!sc.sufficient, "应判定不足");
        assert!(sc.required_gb >= 1_000_000);
        assert!(!sc.filesystem.is_empty(), "应记录文件系统: {sc:?}");
    }

    #[test]
    fn space_check_fast_small_hint_sufficient() {
        let dir = tempdir("sc2");
        let sc = space_check_blocking(
            &HashMap::new(),
            "bitcoin",
            "regtest",
            "fast",
            &format!("{}/d", dir.path),
            false,
        );
        assert!(!sc.blocking, "fast 不阻断");
        assert!(sc.sufficient, "regtest 1GB 应充足: {sc:?}");
    }

    #[test]
    fn required_bytes_txindex_adds_extra() {
        let base = required_bytes_for(&HashMap::new(), "bitcoin", "mainnet", "full", false);
        let with = required_bytes_for(&HashMap::new(), "bitcoin", "mainnet", "full", true);
        assert_eq!(with - base, TXINDEX_EXTRA_GB * 1024 * 1024 * 1024);
    }

    // ---- query 解析 / 校验 ----

    #[test]
    fn percent_decode_handles_space_and_hex() {
        assert_eq!(percent_decode("/tank/my%20data"), "/tank/my data");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("no%zzdecode"), "no%zzdecode", "非法 hex 原样");
        assert_eq!(percent_decode("%41"), "A");
    }

    #[test]
    fn valid_network_and_data_dir_guards() {
        assert!(valid_network("ethereum", "sepolia"));
        assert!(valid_network("bitcoin", "regtest"));
        assert!(!valid_network("solana", "mainnet"));
        assert!(!valid_network("ethereum", "holesky"));

        assert!(valid_data_dir("/tank/blockchain/eth"));
        assert!(!valid_data_dir("relative/path"));
        assert!(!valid_data_dir("/tank/../etc"));
    }

    // ---- 路由 specs ----

    #[test]
    fn route_specs_nine_all_blockchain() {
        let specs = route_specs();
        assert_eq!(specs.len(), 9, "应有 9 条路由: {specs:?}");
        assert!(specs.iter().all(|s| s.handler_component == "blockchain"));
        for s in &specs {
            if s.method == HttpMethod::Post || s.method == HttpMethod::Delete {
                assert!(s.requires_auth, "写操作需鉴权: {}", s.path);
                assert!(s.required_roles.iter().any(|r| r == "admin"));
            }
        }
    }

    // ---- handle：创建/列表/详情/预设/空间预检端点 ----

    #[tokio::test]
    async fn create_node_defaults_and_roundtrip() {
        let (s, d) = state_in_tmp("create");
        // data_dir 显式指到临时目录（默认 /tank/blockchain/<net> 在测试环境
        // 可能不可写；默认值语义由断言外的 rpc_port/client/mode 覆盖）
        let resp = h(
            &s,
            HttpMethod::Post,
            &[],
            "",
            serde_json::json!({
                "name": "BTC 主网", "kind": "bitcoin", "network": "mainnet",
                "data_dir": format!("{}/d", d.path)
            }),
        )
        .await;
        assert_eq!(resp.status, 201, "创建应 201: {}", resp.body);
        assert_eq!(resp.body["node"]["mode"], "fast");
        assert_eq!(resp.body["node"]["client"], "bitcoind");
        assert_eq!(resp.body["node"]["rpc_port"], 8332, "BTC 主网缺省 RPC 8332");
        assert_eq!(resp.body["node"]["p2p_port"], 8333, "BTC 主网缺省 P2P 8333");
        let id = resp.body["node"]["id"].as_str().unwrap().to_string();

        let list = h(&s, HttpMethod::Get, &[], "", serde_json::Value::Null).await;
        assert_eq!(list.status, 200);
        assert_eq!(list.body.as_array().unwrap().len(), 1);

        let detail = h(&s, HttpMethod::Get, &[&id], "", serde_json::Value::Null).await;
        assert_eq!(detail.status, 200);
        assert_eq!(detail.body["node"]["id"], id);
        assert!(detail.body["rpc_url"].as_str().unwrap().contains("8332"));
    }

    #[tokio::test]
    async fn create_eth_mainnet_carries_cl_warning() {
        let (s, _d) = state_in_tmp("clwarn");
        let resp = h(
            &s,
            HttpMethod::Post,
            &[],
            "",
            serde_json::json!({
                "name": "ETH 主网", "kind": "ethereum", "network": "mainnet",
                "mode": "fast",
                "data_dir": "/tmp/nexos-bcn-clwarn"
            }),
        )
        .await;
        assert_eq!(resp.status, 201, "fast 创建不阻断: {}", resp.body);
        assert_eq!(resp.body["requires_consensus_client"], true);
        let ws = resp.body["warnings"].as_array().unwrap();
        assert!(
            ws.iter().any(|w| w.as_str().unwrap().contains("共识客户端")),
            "应带 CL 提示: {ws:?}"
        );
    }

    #[tokio::test]
    async fn create_full_without_space_is_409() {
        let (s, _d) = state_in_tmp("full409");
        s.set_size_override("ethereum/mainnet/full", 1_000_000);
        let resp = h(
            &s,
            HttpMethod::Post,
            &[],
            "",
            serde_json::json!({
                "name": "ETH 存档", "kind": "ethereum", "network": "mainnet",
                "mode": "full", "data_dir": "/tmp/nexos-bcn-full409"
            }),
        )
        .await;
        assert_eq!(resp.status, 409, "空间不足 full 应 409: {}", resp.body);
        assert!(resp.body["error"].as_str().unwrap().contains("空间不足"));
        // fast 同条件不阻断
        let fast = h(
            &s,
            HttpMethod::Post,
            &[],
            "",
            serde_json::json!({
                "name": "ETH 快速", "kind": "ethereum", "network": "mainnet",
                "mode": "fast", "data_dir": "/tmp/nexos-bcn-full409"
            }),
        )
        .await;
        assert_eq!(fast.status, 201, "fast 不阻断: {}", fast.body);
        assert!(fast.body["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w.as_str().unwrap().contains("快速模式不阻断")));
    }

    #[tokio::test]
    async fn create_rejects_bad_inputs() {
        let (s, _d) = state_in_tmp("bad");
        let cases = vec![
            (serde_json::json!({"name": "", "kind": "ethereum", "network": "mainnet"}), 400),
            (serde_json::json!({"name": "x", "kind": "solana", "network": "mainnet"}), 400),
            (
                serde_json::json!({"name": "x", "kind": "ethereum", "network": "mainnet", "mode": "light"}),
                400,
            ),
            (
                serde_json::json!({"name": "x", "kind": "bitcoin", "network": "mainnet", "mode": "fast", "txindex": true}),
                400,
            ),
            (
                serde_json::json!({"name": "x", "kind": "ethereum", "network": "mainnet", "client": "reth"}),
                400,
            ),
            (
                serde_json::json!({"name": "x", "kind": "ethereum", "network": "mainnet", "data_dir": "relative"}),
                400,
            ),
            (
                serde_json::json!({"name": "x", "kind": "ethereum", "network": "mainnet", "rpc_port": 80}),
                400,
            ),
        ];
        for (body, expect) in cases {
            let resp = h(&s, HttpMethod::Post, &[], "", body).await;
            assert_eq!(resp.status, expect, "应 {expect}: {}", resp.body);
        }
    }

    #[tokio::test]
    async fn space_check_endpoint_shapes() {
        let (s, _d) = state_in_tmp("scend");
        let resp = space_check_q(
            &s,
            "kind=ethereum&network=mainnet&mode=full&data_dir=/tmp/nexos-bcn-scend&txindex=0",
        )
        .await;
        assert_eq!(resp.status, 200, "space-check 应 200: {}", resp.body);
        assert_eq!(resp.body["required_gb"], 2200);
        assert!(resp.body["blocking"].as_bool().unwrap());
        assert!(resp.body["available_bytes"].as_u64().unwrap() > 0);
        assert_eq!(
            space_check_q(&s, "kind=foo&network=bar&mode=fast&data_dir=/tmp/x")
                .await
                .status,
            400
        );
    }

    #[tokio::test]
    async fn presets_endpoint_lists_all_with_install_hints() {
        let (s, _d) = state_in_tmp("presets");
        let resp = h(&s, HttpMethod::Get, &["presets"], "", serde_json::Value::Null).await;
        assert_eq!(resp.status, 200);
        let arr = resp.body["presets"].as_array().unwrap();
        assert_eq!(arr.len(), 6, "eth 3 网络 + btc 3 网络 = 6: {arr:?}");
        for p in arr {
            assert!(p["install_hint"].as_str().is_some_and(|h| !h.is_empty()));
            assert!(!p["modes"].as_array().unwrap().is_empty());
        }
    }

    // ---- 生命周期：假二进制冒烟（sleep 脚本代 geth/bitcoind）----

    fn write_fake_bin(dir: &str, name: &str) -> String {
        let p = format!("{dir}/{name}");
        std::fs::write(
            &p,
            format!("#!/bin/sh\necho fake-{name} starting pid=$$\nexec sleep 300\n"),
        )
        .expect("写假二进制失败");
        make_executable(&p);
        p
    }

    fn make_executable(p: &str) {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(p).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(p, perm).unwrap();
    }

    #[tokio::test]
    async fn lifecycle_start_logs_stop_delete_with_fake_bin() {
        let (s, d) = state_in_tmp("life");
        let fake = write_fake_bin(&d.path, "geth");
        s.set_bin_override("geth", &fake);

        let created = h(
            &s,
            HttpMethod::Post,
            &[],
            "",
            serde_json::json!({
                "name": "dev 链", "kind": "ethereum", "network": "dev",
                "data_dir": format!("{}/data", d.path)
            }),
        )
        .await;
        assert_eq!(created.status, 201, "创建: {}", created.body);
        let id = created.body["node"]["id"].as_str().unwrap().to_string();

        let start = h(&s, HttpMethod::Post, &[&id, "start"], "", serde_json::Value::Null).await;
        assert_eq!(start.status, 200, "启动: {}", start.body);
        let pid = start.body["pid"].as_u64().unwrap() as u32;
        assert!(proc_alive(pid), "假二进制应存活");
        assert_eq!(start.body["status"], "running", "dev 网络无同步 → running");

        let again = h(&s, HttpMethod::Post, &[&id, "start"], "", serde_json::Value::Null).await;
        assert_eq!(again.status, 200);
        assert!(again.body["note"].as_str().unwrap().contains("已在运行"));

        // 日志：含启动标记与假二进制 echo 行（子进程写入有毫秒级延迟，轮询兜底）
        let mut saw_fake_line = false;
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let logs =
                h(&s, HttpMethod::Get, &[&id, "logs"], "?tail=10", serde_json::Value::Null).await;
            assert_eq!(logs.status, 200);
            let lines = logs.body["lines"].as_array().unwrap();
            if lines.iter().any(|l| l.as_str().unwrap().contains("fake-geth")) {
                saw_fake_line = true;
                break;
            }
        }
        assert!(
            saw_fake_line,
            "日志应含假二进制启动行（2s 轮询后仍未出现）"
        );

        let stop = h(&s, HttpMethod::Post, &[&id, "stop"], "", serde_json::Value::Null).await;
        assert_eq!(stop.status, 200);
        assert_eq!(stop.body["status"], "stopped");
        assert!(!proc_alive(pid), "停止后进程应退出且已收尸（无僵尸）");

        let del = h(&s, HttpMethod::Delete, &[&id], "", serde_json::Value::Null).await;
        assert_eq!(del.status, 200);
        assert_eq!(del.body["data_dir_kept"], true);
        let gone = h(&s, HttpMethod::Get, &[&id], "", serde_json::Value::Null).await;
        assert_eq!(gone.status, 404);
    }

    #[tokio::test]
    async fn start_without_binary_is_409_with_install_hint() {
        let (s, d) = state_in_tmp("nobin");
        // 覆盖为不存在的路径 → resolve 返回 None（隔离宿主 PATH）
        s.set_bin_override("bitcoind", &format!("{}/nonexistent-bitcoind", d.path));
        let created = h(
            &s,
            HttpMethod::Post,
            &[],
            "",
            serde_json::json!({
                "name": "regtest", "kind": "bitcoin", "network": "regtest",
                "data_dir": format!("{}/d", d.path)
            }),
        )
        .await;
        assert_eq!(created.status, 201);
        assert_eq!(created.body["binary_installed"], false);
        let id = created.body["node"]["id"].as_str().unwrap().to_string();
        let start = h(&s, HttpMethod::Post, &[&id, "start"], "", serde_json::Value::Null).await;
        assert_eq!(start.status, 409, "二进制缺失应 409: {}", start.body);
        let err = start.body["error"].as_str().unwrap();
        assert!(err.contains("未安装"));
        assert!(err.contains("bitcoincore.org"), "应含安装指引: {err}");
    }

    #[tokio::test]
    async fn start_full_insufficient_space_is_409() {
        let (s, d) = state_in_tmp("start409");
        s.set_size_override("bitcoin/mainnet/full", 1_000_000);
        // 假二进制让"未安装"检查通过（空间拦截在其后）
        let fake = write_fake_bin(&d.path, "bitcoind");
        s.set_bin_override("bitcoind", &fake);
        let created = h(
            &s,
            HttpMethod::Post,
            &[],
            "",
            serde_json::json!({
                "name": "btc full", "kind": "bitcoin", "network": "mainnet", "mode": "full",
                "data_dir": format!("{}/d", d.path)
            }),
        )
        .await;
        assert_eq!(created.status, 409, "创建时 full 空间不足已 409");
        // 手工落一行（绕过创建校验）验证 start 侧同样拦截
        let node = ChainNode {
            id: "cn-999".into(),
            name: "手工行".into(),
            kind: "bitcoin".into(),
            network: "mainnet".into(),
            client: "bitcoind".into(),
            mode: "full".into(),
            data_dir: format!("{}/manual", d.path),
            rpc_port: 8332,
            p2p_port: 8333,
            txindex: false,
            extra_flags: String::new(),
            status: "stopped".into(),
            pid: None,
            error: None,
            created_at: "t".into(),
            updated_at: "t".into(),
            last_started_at: None,
            last_command: None,
        };
        s.persist(&node);
        let start =
            h(&s, HttpMethod::Post, &["cn-999", "start"], "", serde_json::Value::Null).await;
        assert_eq!(start.status, 409, "start 全节点空间不足应 409: {}", start.body);
        assert!(start.body["error"].as_str().unwrap().contains("禁止启动"));
    }

    #[tokio::test]
    async fn list_fixes_stale_running_status() {
        let (s, d) = state_in_tmp("fix");
        let node = ChainNode {
            id: "cn-777".into(),
            name: "僵尸行".into(),
            kind: "ethereum".into(),
            network: "sepolia".into(),
            client: "geth".into(),
            mode: "fast".into(),
            data_dir: format!("{}/z", d.path),
            rpc_port: 8555,
            p2p_port: 30304,
            txindex: false,
            extra_flags: String::new(),
            status: "running".into(),
            pid: None,
            error: None,
            created_at: "t".into(),
            updated_at: "t".into(),
            last_started_at: None,
            last_command: None,
        };
        s.persist(&node);
        let list = h(&s, HttpMethod::Get, &[], "", serde_json::Value::Null).await;
        let row = &list.body.as_array().unwrap()[0];
        assert_eq!(
            row["status"], "stopped",
            "无进程登记的 running 应修正为 stopped"
        );
        assert!(row["error"].as_str().unwrap().contains("进程已退出"));
    }

    #[tokio::test]
    async fn bitcoind_start_writes_conf_and_runs() {
        let (s, d) = state_in_tmp("btcconf");
        let fake = write_fake_bin(&d.path, "bitcoind");
        s.set_bin_override("bitcoind", &fake);
        let created = h(
            &s,
            HttpMethod::Post,
            &[],
            "",
            serde_json::json!({
                "name": "regtest", "kind": "bitcoin", "network": "regtest",
                "data_dir": format!("{}/d", d.path)
            }),
        )
        .await;
        assert_eq!(created.status, 201);
        let id = created.body["node"]["id"].as_str().unwrap().to_string();
        let start = h(&s, HttpMethod::Post, &[&id, "start"], "", serde_json::Value::Null).await;
        assert_eq!(start.status, 200, "启动: {}", start.body);
        let conf_path = format!("{}/d/nexos-bitcoin.conf", d.path);
        let conf = std::fs::read_to_string(&conf_path).expect("conf 应已落盘");
        assert!(conf.contains("regtest=1"), "conf: {conf}");
        assert!(conf.contains("prune=550"));
        assert!(start.body["command"].as_str().unwrap().contains("-conf="));
        let _ = h(&s, HttpMethod::Post, &[&id, "stop"], "", serde_json::Value::Null).await;
    }

    // ---- 真实冒烟（--dev / regtest；仅显式设置 SMOKE_*_BIN 时跑，CI 默认跳过）----

    fn smoke_bin(var: &str) -> Option<String> {
        std::env::var(var).ok().filter(|v| !v.trim().is_empty())
    }

    #[tokio::test]
    async fn smoke_geth_dev_lifecycle() {
        let Some(bin) = smoke_bin("SMOKE_GETH_BIN") else {
            eprintln!("[smoke] SMOKE_GETH_BIN 未设置，跳过");
            return;
        };
        let (s, d) = state_in_tmp("smoke-geth");
        s.set_bin_override("geth", &bin);
        let created = h(
            &s,
            HttpMethod::Post,
            &[],
            "",
            serde_json::json!({
                "name": "smoke dev", "kind": "ethereum", "network": "dev",
                "rpc_port": 18546, "p2p_port": 30313,
                "data_dir": format!("{}/d", d.path)
            }),
        )
        .await;
        assert_eq!(created.status, 201, "{}", created.body);
        let id = created.body["node"]["id"].as_str().unwrap().to_string();
        let start = h(&s, HttpMethod::Post, &[&id, "start"], "", serde_json::Value::Null).await;
        assert_eq!(start.status, 200, "geth --dev 启动: {}", start.body);
        // 等 RPC 起来（--dev 秒级；监测 5s 一轮）
        let mut synced = false;
        for _ in 0..24 {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let detail = h(&s, HttpMethod::Get, &[&id], "", serde_json::Value::Null).await;
            if detail.body["node"]["status"] == "running" {
                synced = true;
                break;
            }
        }
        assert!(synced, "geth --dev 应在 12s 内 running");
        let logs =
            h(&s, HttpMethod::Get, &[&id, "logs"], "?tail=50", serde_json::Value::Null).await;
        let joined = logs.body["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!joined.is_empty(), "geth 日志应非空");
        let _ = h(&s, HttpMethod::Post, &[&id, "stop"], "", serde_json::Value::Null).await;
        let _ = h(&s, HttpMethod::Delete, &[&id], "", serde_json::Value::Null).await;
    }

    #[tokio::test]
    async fn smoke_bitcoind_regtest_lifecycle() {
        let Some(bin) = smoke_bin("SMOKE_BITCOIND_BIN") else {
            eprintln!("[smoke] SMOKE_BITCOIND_BIN 未设置，跳过");
            return;
        };
        let (s, d) = state_in_tmp("smoke-btc");
        s.set_bin_override("bitcoind", &bin);
        let created = h(
            &s,
            HttpMethod::Post,
            &[],
            "",
            serde_json::json!({
                "name": "smoke regtest", "kind": "bitcoin", "network": "regtest",
                "rpc_port": 18443, "p2p_port": 18444,
                "data_dir": format!("{}/d", d.path)
            }),
        )
        .await;
        assert_eq!(created.status, 201, "{}", created.body);
        let id = created.body["node"]["id"].as_str().unwrap().to_string();
        let start = h(&s, HttpMethod::Post, &[&id, "start"], "", serde_json::Value::Null).await;
        assert_eq!(start.status, 200, "bitcoind regtest 启动: {}", start.body);
        let pid = start.body["pid"].as_u64().unwrap() as u32;
        // bitcoind 初始化 ~1s：确认进程真实存活（防"spawn 即退"的假绿）
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            proc_alive(pid),
            "bitcoind 应在 2s 后仍存活（若秒退说明 conf/参数被拒，见日志）"
        );
        let logs_mid =
            h(&s, HttpMethod::Get, &[&id, "logs"], "?tail=80", serde_json::Value::Null).await;
        let mid = logs_mid.body["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l.as_str().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            mid.contains("regtest") || mid.contains("Bitcoin Core"),
            "日志应含 bitcoind 输出（regtest 标识）: {mid}"
        );
        let stop = h(&s, HttpMethod::Post, &[&id, "stop"], "", serde_json::Value::Null).await;
        assert_eq!(stop.status, 200);
        assert!(!proc_alive(pid), "停止后进程应退出且已收尸");
        let _ = h(&s, HttpMethod::Delete, &[&id], "", serde_json::Value::Null).await;
    }
}
