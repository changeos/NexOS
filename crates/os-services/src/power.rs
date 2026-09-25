//! 电源 / UPS / 硬件监控（规划文档 §3.16 power 组件）
//!
//! 职责：
//! - UPS 状态查询（在线/离线、电池电量、续航估算）
//! - 温度 / 风扇读取
//! - SMART 健康检查
//! - 电源调度（定时开关机）
//! - 强制关机（断电保护 ZFS——卸载池前下电）

use os_core::{Deserialize, Serialize};

use crate::ServiceError;

// ----------------------------------------------------------------------------
// UPS
// ----------------------------------------------------------------------------

/// UPS 状态
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpsStatus {
    /// 是否在线（市电正常）
    pub online: bool,
    /// 电池电量百分比（0-100；None = 未知）
    pub battery_level: Option<u8>,
    /// 预计续航分钟（None = 未知）
    pub estimated_minutes: Option<u32>,
    /// UPS 型号
    pub model: String,
}

// ----------------------------------------------------------------------------
// 风扇 / 温度
// ----------------------------------------------------------------------------

/// 风扇读数
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FanReading {
    /// 标签（如 `"cpu_fan"` / `"sys_fan1"`）
    pub label: String,
    /// 转速（RPM）
    pub rpm: u32,
}

/// 温度读数
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TempReading {
    /// 标签（如 `"cpu"` / `"disk_sda"`）
    pub label: String,
    /// 温度（摄氏度）
    pub celsius: f32,
}

// ----------------------------------------------------------------------------
// SMART
// ----------------------------------------------------------------------------

/// SMART 健康报告
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartReport {
    /// 磁盘标识（如 `"/dev/sda"`）
    pub disk: String,
    /// SMART 总体是否通过
    pub passed: bool,
    /// 温度（摄氏度）
    pub temperature: f32,
    /// 重映射扇区数（越高越危险）
    pub reallocated_sectors: u64,
    /// 通电小时数
    pub power_on_hours: u64,
}

// ----------------------------------------------------------------------------
// 电源调度
// ----------------------------------------------------------------------------

/// 电源调度（cron 表达式，格式见 backup::CronExpr）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PowerSchedule {
    /// 定时开机（cron；None = 不调度）
    pub power_on_cron: Option<String>,
    /// 定时关机（cron；None = 不调度）
    pub shutdown_cron: Option<String>,
}

// ----------------------------------------------------------------------------
// PowerManager trait（async）
// ----------------------------------------------------------------------------

/// 电源 / 硬件管理器——UPS、温风、SMART、调度、强制关机。
#[allow(async_fn_in_trait)]
pub trait PowerManager: Send + Sync {
    /// 查询 UPS 状态。
    async fn ups_status(&self) -> Result<UpsStatus, ServiceError>;

    /// 读取所有温度传感器。
    async fn read_temps(&self) -> Result<Vec<TempReading>, ServiceError>;

    /// 读取所有风扇转速。
    async fn read_fans(&self) -> Result<Vec<FanReading>, ServiceError>;

    /// 对指定磁盘做 SMART 检查。
    async fn smart_check(&self, disk: &str) -> Result<SmartReport, ServiceError>;

    /// 设置电源调度（定时开关机）。
    async fn schedule_power(&self, sched: PowerSchedule) -> Result<(), ServiceError>;

    /// 强制关机（断电保护：先卸载 ZFS 池再下电；reason 记入审计日志）。
    async fn force_shutdown(&self, reason: &str) -> Result<(), ServiceError>;
}
