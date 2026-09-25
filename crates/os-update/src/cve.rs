//! CVE 监听（规划文档 §3.12）
//!
//! 监听系统所含 C 依赖（Samba / QEMU / rdma-core 等）的安全公告，
//! 命中后联动更新引擎与 IM 通知。

use async_trait::async_trait;
use os_core::DateTime;
use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// 公告与严重级别
// ----------------------------------------------------------------------------

/// CVE 严重级别
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CveSeverity {
    /// 低
    Low,
    /// 中
    Medium,
    /// 高
    High,
    /// 严重
    Critical,
}

/// 单条 CVE 公告
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CveAdvisory {
    /// CVE 编号（如 `CVE-2024-12345`）
    pub cve_id: String,
    /// 受影响组件（如 `samba` / `qemu` / `rdma-core`）
    pub affected_component: String,
    /// 严重级别
    pub severity: CveSeverity,
    /// 已修复版本
    pub fixed_version: String,
    /// 发布时间（UTC）
    pub published_at: DateTime,
}

// ----------------------------------------------------------------------------
// 回调与监听 trait
// ----------------------------------------------------------------------------

/// CVE 公告回调（收到新公告时触发）
///
/// 实现者：可联动 IM 通知 / 自动触发更新。经 `Box<dyn CveCallback>` 注册到
/// CveMonitor，故用 `#[async_trait]` 保证 dyn 兼容（ADR-COMPAT-001）。
#[async_trait]
pub trait CveCallback: Send + Sync {
    /// 收到一条新公告。
    async fn on_advisory(&self, advisory: &CveAdvisory);
}

/// CVE 监听器——轮询/订阅上游安全公告。
///
/// 实现者：`NvdCveMonitor`（默认，对接 NVD/OSV 数据源）。
#[allow(async_fn_in_trait)]
pub trait CveMonitor: Send + Sync {
    /// 主动检查当前组件受影响的公告列表。
    async fn check_advisories(&self) -> Result<Vec<CveAdvisory>, crate::UpdateError>;

    /// 订阅新公告回调（多条回调链式注册）。
    async fn subscribe(&self, callback: Box<dyn CveCallback>);
}
