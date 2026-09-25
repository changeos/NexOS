//! 领域模型与通用类型
//!
//! 这里的结构体是跨 crate 共享的"领域通用模型"（如 Health、Capacity、NodeInfo）。
//! 各业务的详细模型（如 ZFS 的 VdevSpec、VM 的 CpuTopology）放各自 crate，
//! 仅把"被多个 crate 共享"的部分放此。

use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// Health / 状态（被 osd/os-monitor/几乎所有组件复用）
// ----------------------------------------------------------------------------

/// 组件/资源健康状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    /// 健康
    Healthy,
    /// 降级（部分功能不可用，如 RPC 不可用但 fallback 生效）
    Degraded,
    /// 不健康（故障）
    Unhealthy,
    /// 未知（探测超时/未启动）
    Unknown,
}

/// 健康探测结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthReport {
    pub health: Health,
    /// 人类可读详情（如 "RPC timeout, fallback to remote"）
    pub message: Option<String>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

// ----------------------------------------------------------------------------
// 容量 / 资源规格（被 storage/compute/network 复用）
// ----------------------------------------------------------------------------

/// 容量（字节）
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Capacity {
    pub used_bytes: u64,
    pub total_bytes: u64,
}
impl Capacity {
    pub fn free_bytes(&self) -> u64 {
        self.total_bytes.saturating_sub(self.used_bytes)
    }
    pub fn used_ratio(&self) -> f64 {
        if self.total_bytes == 0 {
            0.0
        } else {
            self.used_bytes as f64 / self.total_bytes as f64
        }
    }
}

/// 资源配额（cgroup v2：CPU/内存/IO，见 §3.13 osd）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceQuota {
    /// CPU 配额（如 0.5 = 半核；None = 不限）
    pub cpu_cores: Option<f32>,
    /// 内存上限（字节；None = 不限）
    pub memory_bytes: Option<u64>,
    /// IO 带宽上限（字节/秒；None = 不限）
    pub io_bps_limit: Option<u64>,
}

// ----------------------------------------------------------------------------
// 节点 / 集群（被 os-meta/os-discover/os-provision 复用）
// ----------------------------------------------------------------------------

/// 集群节点角色
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeRole {
    /// HA 集群 leader（openraft）
    Leader,
    /// HA 集群 follower（法定成员）
    Follower,
    /// 同级 peer（不进法定，仅 ZFS mirror 同步，见 §3.5）
    Peer,
    /// 独立单节点
    Standalone,
}

/// 节点基本信息（发现/集群成员共用）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub node_id: crate::NodeId,
    pub role: NodeRole,
    pub version: String,
    pub arch: String,
    /// 管理网地址（含端口）
    pub endpoints: Vec<String>,
    pub health: Health,
}

// ----------------------------------------------------------------------------
// 分页（API 通用）
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageRequest {
    pub offset: u32,
    pub limit: u32,
}
impl Default for PageRequest {
    fn default() -> Self {
        Self {
            offset: 0,
            limit: 50,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageResponse<T> {
    pub items: Vec<T>,
    pub total: u32,
    pub offset: u32,
    pub limit: u32,
}

// ----------------------------------------------------------------------------
// 命令执行结果（被 os-storage / os-compute / os-services 复用）
// ----------------------------------------------------------------------------

/// 子进程命令执行结果（与 `std::process::Output` 同构，但 owned + 可由测试构造）。
///
/// **统一来源**：之前 `os-storage::backend_impl::CommandOutput` /
/// `os-compute::apt::CommandOutput` / `os-services::media_ffmpeg::FfmpegOutput`
/// 三处独立定义同构结构（review2 P-R2-1）。现统一到此，避免字段演进时三处脱节。
///
/// 字段名用 `exit_code`（与 `std::os::unix::ExitStatusExt::code` 语义一致）；
/// 退出码 `0` 表示成功，`-1` 表示进程被信号杀、无码（与各原实现约定一致）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandOutput {
    /// stdout（UTF-8 解码后的字符串；lossy 不会失败）
    pub stdout: String,
    /// stderr（UTF-8 解码后的字符串；保留供错误诊断）
    pub stderr: String,
    /// 退出码（0 = 成功；-1 = 进程被信号杀，无码）
    pub exit_code: i32,
}

impl CommandOutput {
    /// 成功、空输出的便捷构造（测试用）。
    pub fn ok() -> Self {
        Self {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        }
    }

    /// 成功并带 stdout（测试 / fixture 用）。
    pub fn ok_with_stdout(stdout: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: 0,
        }
    }

    /// 失败构造（非零退出码 + stderr）。
    pub fn fail(exit_code: i32, stderr: impl Into<String>) -> Self {
        Self {
            stdout: String::new(),
            stderr: stderr.into(),
            exit_code,
        }
    }

    /// 是否成功（退出码 == 0）。
    pub fn is_success(&self) -> bool {
        self.exit_code == 0
    }
}

impl Default for CommandOutput {
    fn default() -> Self {
        Self::ok()
    }
}
