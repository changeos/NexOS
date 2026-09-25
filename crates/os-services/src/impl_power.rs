//! `LinuxPowerManager` —— 电源 / UPS / 硬件监控默认实现（规划文档 §3.16 power 组件）。
//!
//! 实现策略（无依赖骨架）：
//! - UPS：编排 `upsc <name>@<host>`（NUT 协议客户端），输出为 key-value 行；
//!   `parse_upsc_output` 为纯函数，可用 fixture 单测。
//! - 温度 / 风扇：编排 `sensors -u`（lm-sensors），`parse_sensors_output` 为纯函数。
//! - SMART：编排 `smartctl -a -j <disk>`，输出 JSON；`parse_smartctl_json` 为纯函数。
//! - 调度：`schedule_power` 仅持久化配置（内存态）+ 记录预期 RTC 唤醒语义；
//!   真正的 `rtcwake` 调用留待集成（高危，不在骨架执行）。
//! - 强制关机：`force_shutdown` 仅记录审计日志（写入内部 ring buffer），**不真关机**
//!   （红线：骨架 / 测试环境严禁真关机）。生产侧由上层编排 `shutdown -h`。
//!
//! 断电保护决策（高价值纯算法）：
//! - `UpsShutdownConfig`：阈值配置（电量下限、续航下限、是否要求两者皆满足）。
//! - `decide_ups_shutdown`：给定 `UpsStatus` + 配置 → `UpsShutdownDecision`。
//!   默认策略 `battery<30% && remaining<10min → graceful shutdown`（见规格书 §3）。
//!
//! SMART 健康判定（高价值纯算法）：
//! - `SmartAttribute`：单条 SMART 属性（ID/raw/threshold/worst）。
//! - `parse_smartctl_json`：解析 smartctl JSON，得 `SmartReport` + 属性表。
//! - `assess_smart_health`：关键属性（reallocated/pending/offline-uncorrectable）
//!   超阈值 → `SmartHealth::Degraded`/`Failed`。

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Mutex;

use tokio::process::Command;

use crate::power::{FanReading, PowerManager, PowerSchedule, SmartReport, TempReading, UpsStatus};
use crate::ServiceError;

// ============================================================
// UPS 断电保护决策（纯算法）
// ============================================================

/// UPS 断电保护阈值配置。
///
/// 默认策略（规格书 §3）：`battery_level < 30% && estimated_minutes < 10 → graceful shutdown`。
/// 通过 `require_both = false` 可改为「任一条件满足即触发」（更激进）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpsShutdownConfig {
    /// 电池电量下限（百分比，低于此值视为危险）。默认 30。
    pub battery_threshold: u8,
    /// 续航下限（分钟，低于此值视为危险）。默认 10。
    pub minutes_threshold: u32,
    /// true：两个条件**都**满足才触发（保守，默认）；false：任一满足即触发（激进）。
    pub require_both: bool,
}

impl Default for UpsShutdownConfig {
    fn default() -> Self {
        Self {
            battery_threshold: 30,
            minutes_threshold: 10,
            require_both: true,
        }
    }
}

/// UPS 断电保护决策结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsShutdownDecision {
    /// 市电正常，无需动作。
    Online,
    /// 仍在安全范围内，继续观察。
    Safe,
    /// 触发优雅关机（理由附在字段中由调用方组合审计文案）。
    Shutdown {
        /// 触发的电量百分比（决策时刻快照）
        battery: Option<u8>,
        /// 触发的续航分钟（决策时刻快照）
        minutes: Option<u32>,
    },
}

/// 给定 UPS 状态 + 阈值配置，决定是否触发断电保护关机。纯函数。
///
/// 规则：
/// - `online = true` → `Online`（市电正常，绝不触发）。
/// - 电量 / 续航任一为 `None`（未知）→ 按 `require_both` 退化：
///     - 已知量达危险值即视作危险（避免 UPS 不上报时反而漏保护）。
/// - 否则按阈值比较 + `require_both` 合取/析取。
pub fn decide_ups_shutdown(status: &UpsStatus, cfg: &UpsShutdownConfig) -> UpsShutdownDecision {
    if status.online {
        return UpsShutdownDecision::Online;
    }
    let bat_low = status
        .battery_level
        .map(|b| b < cfg.battery_threshold)
        // 电量未知时视为危险（保守，避免漏保护）。
        .unwrap_or(true);
    let min_low = status
        .estimated_minutes
        .map(|m| m < cfg.minutes_threshold)
        .unwrap_or(true);

    let should_shutdown = if cfg.require_both {
        bat_low && min_low
    } else {
        bat_low || min_low
    };

    if should_shutdown {
        UpsShutdownDecision::Shutdown {
            battery: status.battery_level,
            minutes: status.estimated_minutes,
        }
    } else {
        UpsShutdownDecision::Safe
    }
}

// ============================================================
// SMART 属性与健康判定（纯算法）
// ============================================================

/// 单条 SMART 属性（smartctl `ata_smart_attributes.table[]` 一行）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SmartAttribute {
    /// 属性 ID（如 5 = Reallocated_Sector_Ct）。
    pub id: u32,
    /// 名称（如 `"Reallocated_Sector_Ct"`）。
    pub name: String,
    /// 原始值（raw_value，用于退化判定）。
    pub raw: u64,
    /// worst 值（归一化最差）。
    pub worst: u32,
    /// threshold 值（归一化阈值）。
    pub threshold: u32,
}

/// SMART 健康等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmartHealth {
    /// 健康（passed && 关键属性 raw 均为 0）。
    Healthy,
    /// 退化（关键属性 raw > 0 但未超倍率阈值；passed 仍 true）。
    Degraded,
    /// 故障（passed = false 或关键属性 raw 超阈值）。
    Failed,
}

/// 关键属性 ID（任一非 0 即视作退化信号）。
///
/// - 5  Reallocated_Sector_Ct（重映射扇区）
/// - 197 Current_Pending_Sector（待重映射）
/// - 198 Offline_Uncorrectable（离线不可纠正）
const CRITICAL_ATTR_IDS: [u32; 3] = [5, 197, 198];

/// 单个关键属性的退化阈值（raw 超过此值即 Failed，否则 Degraded）。
const FAILED_RAW_LIMIT: u64 = 100;

/// 根据 SMART 报告 + 属性表判定健康等级。纯函数。
pub fn assess_smart_health(report: &SmartReport, attrs: &[SmartAttribute]) -> SmartHealth {
    if !report.passed {
        return SmartHealth::Failed;
    }
    let mut degraded = false;
    for id in CRITICAL_ATTR_IDS {
        if let Some(a) = attrs.iter().find(|a| a.id == id) {
            if a.raw > FAILED_RAW_LIMIT {
                return SmartHealth::Failed;
            }
            if a.raw > 0 {
                degraded = true;
            }
        }
    }
    if degraded {
        SmartHealth::Degraded
    } else {
        SmartHealth::Healthy
    }
}

// ============================================================
// upsc（NUT）输出解析 —— 纯函数
// ============================================================

/// 解析 `upsc <ups>@<host>` 的 key-value 输出，得 `UpsStatus`。纯函数。
///
/// 关心的键（NUT 标准 driver var）：
/// - `ups.status`：`OL` = 市电在线；`OB` = 电池供电；`LB` = 低电量。
/// - `battery.charge`：电量百分比（0-100）。
/// - `battery.runtime`：续航秒数（÷60 得分钟）。
/// - `ups.model` / `device.model`：型号。
///
/// 未识别的键忽略；缺失的 `ups.status` 视为离线（保守）。
pub fn parse_upsc_output(output: &str) -> UpsStatus {
    let mut map: HashMap<&str, &str> = HashMap::new();
    for line in output.lines() {
        let line = line.trim();
        if let Some((k, v)) = line.split_once(':') {
            map.insert(k.trim(), v.trim());
        }
    }

    // ups.status 可能是空格分隔的多 token（"OL" / "OB LB" 等）。
    let status_raw = map.get("ups.status").copied().unwrap_or("OB");
    let online = status_raw.split_whitespace().any(|tok| tok == "OL");

    let battery_level = map
        .get("battery.charge")
        .and_then(|v| v.parse::<u8>().ok())
        .map(|n| n.min(100));

    let estimated_minutes = map
        .get("battery.runtime")
        .and_then(|v| v.parse::<u32>().ok())
        .map(|secs| secs / 60);

    let model = map
        .get("ups.model")
        .or_else(|| map.get("device.model"))
        .map(|s| (*s).to_string())
        .unwrap_or_else(|| "unknown".to_string());

    UpsStatus {
        online,
        battery_level,
        estimated_minutes,
        model,
    }
}

// ============================================================
// sensors（lm-sensors）输出解析 —— 纯函数
// ============================================================

/// 解析 `sensors -u` 输出，提取风扇（`fan\d_input` → RPM）与温度（`temp\d_input` → °C）。
///
/// `sensors -u` 输出形如：
/// ```text
/// coretemp-isa-0000
/// Adapter: ISA adapter
/// Package id 0:  +45.0°C  (high = +80.0°C)
/// temp1_input: 45.000
/// temp1_max: 80.000
///
/// it8728-isa-0a30
/// fan1_input: 1245
/// fan1_min: 0
/// ```
/// 本函数按「块」（空行分隔）+「最近见到的非 `_input:` 标签行」为 sensor 命名。
/// 仅解析数值合法的项；非法行忽略。
pub fn parse_sensors_output(output: &str) -> (Vec<TempReading>, Vec<FanReading>) {
    let mut temps = Vec::new();
    let mut fans = Vec::new();
    // 当前块的「人类标签」候选：取块内第一个冒号结尾、含字母的描述行。
    let mut current_label: Option<String> = None;

    for raw in output.lines() {
        let line = raw.trim();
        if line.is_empty() {
            current_label = None;
            continue;
        }
        // 描述行形如 "Package id 0:  +45.0°C  (...)" —— 含 ':' 且非 key:value 数值行。
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim();
            let v = v.trim();
            // 数值键：fooN_input: <number>
            if let Some(suffix) = k
                .strip_prefix("temp")
                .and_then(|s| s.strip_suffix("_input"))
            {
                if suffix.chars().all(|c| c.is_ascii_digit()) {
                    if let Ok(c) = v.parse::<f32>() {
                        let label = current_label
                            .clone()
                            .unwrap_or_else(|| format!("temp{suffix}"));
                        temps.push(TempReading { label, celsius: c });
                    }
                    continue;
                }
            }
            if let Some(suffix) = k.strip_prefix("fan").and_then(|s| s.strip_suffix("_input")) {
                if suffix.chars().all(|c| c.is_ascii_digit()) {
                    if let Ok(rpm) = v.parse::<u32>() {
                        let label = current_label
                            .clone()
                            .unwrap_or_else(|| format!("fan{suffix}"));
                        fans.push(FanReading { label, rpm });
                    }
                    continue;
                }
            }
            // 否则视作描述行（如 "Package id 0"），记为候选标签。
            if !k.is_empty() && v.parse::<f32>().is_err() {
                current_label = Some(k.to_string());
            }
        }
    }

    (temps, fans)
}

// ============================================================
// smartctl -j 输出解析 —— 纯函数
// ============================================================

/// 解析 `smartctl -a -j <disk>` 的 JSON 输出，得 (`SmartReport`, 属性表)。纯函数。
///
/// 关心字段：
/// - `smart_status.passed`（bool）→ `SmartReport.passed`
/// - `temperature.current` → `temperature`
/// - `ata_smart_attributes.table[]`：id/name/raw/value/worst/thresh
///   （reallocated = id 5；power_on_hours = id 9）
///
/// 解析容错：缺字段返回合理默认（passed=false 当无 smart_status；温度 0 当无）。
/// 非 SATA 盘（无 ata_smart_attributes）属性表为空，reallocated/power_on 从 NVMe 等字段
/// 退化取值（本骨架仅支持 SATA 属性表，缺则 0）。
pub fn parse_smartctl_json(
    disk: &str,
    json: &str,
) -> Result<(SmartReport, Vec<SmartAttribute>), ServiceError> {
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| ServiceError::HardwareError(format!("smartctl JSON 解析失败: {e}")))?;

    let passed = v
        .get("smart_status")
        .and_then(|s| s.get("passed"))
        .and_then(|p| p.as_bool())
        .unwrap_or(false);

    let temperature = v
        .get("temperature")
        .and_then(|t| t.get("current"))
        .and_then(|c| c.as_f64())
        .map(|n| n as f32)
        .unwrap_or(0.0);

    let mut reallocated_sectors: u64 = 0;
    let mut power_on_hours: u64 = 0;
    let mut attrs = Vec::new();

    if let Some(table) = v
        .get("ata_smart_attributes")
        .and_then(|a| a.get("table"))
        .and_then(|t| t.as_array())
    {
        for row in table {
            let id = row.get("id").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            let name = row
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let raw = row
                .get("raw")
                .and_then(|r| r.get("value"))
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            let value = row.get("value").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            let worst = row.get("worst").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            let threshold = row.get("thresh").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            let _ = value; // 归一化 value 暂不入结构（保留以备扩展）

            if id == 5 {
                reallocated_sectors = raw;
            }
            if id == 9 {
                power_on_hours = raw;
            }
            attrs.push(SmartAttribute {
                id,
                name,
                raw,
                worst,
                threshold,
            });
        }
    }

    let report = SmartReport {
        disk: disk.to_string(),
        passed,
        temperature,
        reallocated_sectors,
        power_on_hours,
    };
    Ok((report, attrs))
}

// ============================================================
// cron 校验（轻量；不引入第三方 cron crate）
// ============================================================

/// 校验 5 段 cron 表达式（分 时 日 月 周）是否合法。纯函数。
///
/// 支持通配 `*`、单值、逗号列表、`a-b` 范围、`*/n` 步进。
/// 不支持的语法（如 `L`/`W`/`#`）返回 false。本骨架仅用于 `schedule_power` 入参校验。
pub fn is_valid_cron(expr: &str) -> bool {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return false;
    }
    const RANGES: [(u32, u32); 5] = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 7)];
    parts
        .iter()
        .zip(RANGES.iter())
        .all(|(field, (lo, hi))| validate_cron_field(field, *lo, *hi))
}

fn validate_cron_field(field: &str, lo: u32, hi: u32) -> bool {
    for term in field.split(',') {
        if !validate_cron_term(term, lo, hi) {
            return false;
        }
    }
    true
}

fn validate_cron_term(term: &str, lo: u32, hi: u32) -> bool {
    // */n
    if let Some(step) = term.strip_prefix("*/") {
        return step.parse::<u32>().map(|n| n >= 1).unwrap_or(false);
    }
    // a-b 或 a-b/n
    let (range_part, step_opt) = match term.split_once('/') {
        Some((r, s)) => (r, Some(s)),
        None => (term, None),
    };
    let (start, end) = if range_part == "*" {
        (lo, hi)
    } else if let Some((a, b)) = range_part.split_once('-') {
        match (a.parse::<u32>(), b.parse::<u32>()) {
            (Ok(a), Ok(b)) => (a, b),
            _ => return false,
        }
    } else {
        match range_part.parse::<u32>() {
            Ok(n) => (n, n),
            Err(_) => return false,
        }
    };
    if !(lo <= start && start <= hi) {
        return false;
    }
    if !(lo <= end && end <= hi) {
        return false;
    }
    if start > end {
        return false;
    }
    if let Some(s) = step_opt {
        if s.parse::<u32>().map(|n| n >= 1).unwrap_or(false) {
            return true;
        }
        return false;
    }
    true
}

// ============================================================
// LinuxPowerManager
// ============================================================

/// 内部审计日志条目（force_shutdown 等高危动作记录）。
#[derive(Debug, Clone)]
pub struct AuditEntry {
    /// 动作（如 `"force_shutdown"` / `"schedule_power"`）。
    pub action: String,
    /// 原因 / 参数。
    pub detail: String,
    /// 时间戳（Unix 秒；骨架用 SystemTime，避免引入 chrono 依赖到此处）。
    pub timestamp_secs: u64,
}

/// 默认 `PowerManager` 实现：编排 NUT / lm-sensors / smartctl CLI。
///
/// 设计要点：
/// - 所有 CLI 调用经 `tokio::process::Command`，stderr 合并；命令不存在 / 非零退出
///   → `HardwareError`。
/// - `force_shutdown` / `schedule_power` 在骨架中**不执行真实系统变更**
///   （红线：测试环境严禁真关机 / 真 rtcwake）。仅记录审计日志到内部 ring buffer，
///   供测试断言。生产侧由上层编排器（osd）替换为真实 `shutdown -h` / `rtcwake`。
pub struct LinuxPowerManager {
    /// UPS 标识（`upsc` 参数，形如 `"ups@localhost"`）。
    ups_target: String,
    /// 审计日志（ring buffer，最近 N 条）。
    audit: Mutex<Vec<AuditEntry>>,
    /// 当前持久化的电源调度配置（内存态）。
    schedule: Mutex<Option<PowerSchedule>>,
    /// 若 `Some`，`force_shutdown` / CLI 调用将短路返回此错误（测试注入；不暴露 pub 构造）。
    #[allow(dead_code)]
    dry_run: bool,
}

impl LinuxPowerManager {
    /// 新建：`ups_target` 形如 `"ups@localhost"`。`dry_run = true` 时跳过真实 CLI
    /// （返回未配置错误，避免单元测误调宿主命令）。
    pub fn new(ups_target: impl Into<String>) -> Self {
        Self {
            ups_target: ups_target.into(),
            audit: Mutex::new(Vec::new()),
            schedule: Mutex::new(None),
            dry_run: true,
        }
    }

    /// 取最近的审计日志（拷贝），供测试断言。
    pub fn audit_log(&self) -> Vec<AuditEntry> {
        self.audit.lock().expect("audit lock").clone()
    }

    /// 取当前电源调度配置（拷贝）。
    pub fn current_schedule(&self) -> Option<PowerSchedule> {
        self.schedule.lock().expect("schedule lock").clone()
    }

    fn record_audit(&self, action: impl Into<String>, detail: impl Into<String>) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let mut g = self.audit.lock().expect("audit lock");
        if g.len() >= 64 {
            g.remove(0); // ring buffer
        }
        g.push(AuditEntry {
            action: action.into(),
            detail: detail.into(),
            timestamp_secs: ts,
        });
    }

    /// 运行命令并返回合并的 stdout（dry_run 下返回 HardwareError）。
    async fn run_cmd(&self, program: &str, args: &[&str]) -> Result<String, ServiceError> {
        if self.dry_run {
            return Err(ServiceError::HardwareError(format!(
                "dry_run：未执行 `{program}`（骨架默认不调宿主命令）"
            )));
        }
        let output = Command::new(program)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| ServiceError::HardwareError(format!("启动 `{program}` 失败: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ServiceError::HardwareError(format!(
                "`{program}` 退出码 {:?}: {stderr}",
                output.status.code()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

impl LinuxPowerManager {
    /// 公开便捷入口：依据 UPS 状态做断电保护决策（包装纯函数）。
    pub fn shutdown_decision(&self, status: &UpsStatus) -> UpsShutdownDecision {
        decide_ups_shutdown(status, &UpsShutdownConfig::default())
    }
}

#[allow(async_fn_in_trait)]
impl PowerManager for LinuxPowerManager {
    async fn ups_status(&self) -> Result<UpsStatus, ServiceError> {
        let out = self.run_cmd("upsc", &[&self.ups_target]).await?;
        Ok(parse_upsc_output(&out))
    }

    async fn read_temps(&self) -> Result<Vec<TempReading>, ServiceError> {
        let out = self.run_cmd("sensors", &["-u"]).await?;
        let (temps, _) = parse_sensors_output(&out);
        Ok(temps)
    }

    async fn read_fans(&self) -> Result<Vec<FanReading>, ServiceError> {
        let out = self.run_cmd("sensors", &["-u"]).await?;
        let (_, fans) = parse_sensors_output(&out);
        Ok(fans)
    }

    async fn smart_check(&self, disk: &str) -> Result<SmartReport, ServiceError> {
        let out = self.run_cmd("smartctl", &["-a", "-j", disk]).await?;
        let (report, _attrs) = parse_smartctl_json(disk, &out)?;
        Ok(report)
    }

    async fn schedule_power(&self, sched: PowerSchedule) -> Result<(), ServiceError> {
        // 校验 cron（若存在）。
        if let Some(c) = &sched.power_on_cron {
            if !is_valid_cron(c) {
                return Err(ServiceError::Internal(format!("非法 power_on_cron: {c}")));
            }
        }
        if let Some(c) = &sched.shutdown_cron {
            if !is_valid_cron(c) {
                return Err(ServiceError::Internal(format!("非法 shutdown_cron: {c}")));
            }
        }
        // 骨架：仅持久化内存 + 记审计；真实 rtcwake / cron 安装由上层编排。
        let detail = format!(
            "power_on={:?} shutdown={:?}",
            sched.power_on_cron, sched.shutdown_cron
        );
        self.record_audit("schedule_power", detail);
        *self.schedule.lock().expect("schedule lock") = Some(sched);
        Ok(())
    }

    async fn force_shutdown(&self, reason: &str) -> Result<(), ServiceError> {
        // 红线：骨架 / 测试环境绝不真关机。仅记审计；生产侧由 osd 替换实现。
        // 期望执行顺序由上层保证：sync → 等 ZFS txg 落盘 → shutdown -h。
        self.record_audit("force_shutdown", reason);
        // dry_run 下显式告知未真执行；非 dry_run 亦不在此直接发 shutdown（高危，
        // 由可信上层编排）。
        Err(ServiceError::HardwareError(format!(
            "force_shutdown 已记录审计但未执行（骨架安全策略）: {reason}"
        )))
    }
}

// ============================================================
// 单元测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::power::UpsStatus;

    fn ups(online: bool, bat: Option<u8>, min: Option<u32>) -> UpsStatus {
        UpsStatus {
            online,
            battery_level: bat,
            estimated_minutes: min,
            model: "TestUPS".into(),
        }
    }

    // ---- UPS 决策 ----

    #[test]
    fn ups_decision_online_never_triggers() {
        let cfg = UpsShutdownConfig::default();
        let s = ups(true, Some(0), Some(0));
        assert_eq!(decide_ups_shutdown(&s, &cfg), UpsShutdownDecision::Online);
    }

    #[test]
    fn ups_decision_default_requires_both() {
        let cfg = UpsShutdownConfig::default(); // bat<30 && min<10
        assert_eq!(
            decide_ups_shutdown(&ups(false, Some(50), Some(5)), &cfg),
            UpsShutdownDecision::Safe
        );
        assert_eq!(
            decide_ups_shutdown(&ups(false, Some(20), Some(30)), &cfg),
            UpsShutdownDecision::Safe
        );
        assert_eq!(
            decide_ups_shutdown(&ups(false, Some(20), Some(5)), &cfg),
            UpsShutdownDecision::Shutdown {
                battery: Some(20),
                minutes: Some(5)
            }
        );
    }

    #[test]
    fn ups_decision_any_mode_triggers_on_either() {
        let cfg = UpsShutdownConfig {
            require_both: false,
            ..Default::default()
        };
        assert_eq!(
            decide_ups_shutdown(&ups(false, Some(20), Some(30)), &cfg),
            UpsShutdownDecision::Shutdown {
                battery: Some(20),
                minutes: Some(30)
            }
        );
        assert_eq!(
            decide_ups_shutdown(&ups(false, Some(80), Some(5)), &cfg),
            UpsShutdownDecision::Shutdown {
                battery: Some(80),
                minutes: Some(5)
            }
        );
    }

    #[test]
    fn ups_decision_unknown_battery_treated_as_dangerous() {
        let cfg = UpsShutdownConfig::default();
        // 电量未知、续航低 → 默认 require_both 下：bat_low=true(未知)，min_low=true → Shutdown
        assert_eq!(
            decide_ups_shutdown(&ups(false, None, Some(5)), &cfg),
            UpsShutdownDecision::Shutdown {
                battery: None,
                minutes: Some(5)
            }
        );
    }

    // ---- upsc 解析 ----

    #[test]
    fn parse_upsc_online() {
        let out = "\
ups.status: OL
battery.charge: 95
battery.runtime: 2400
ups.model: Smart-UPS 1500
";
        let s = parse_upsc_output(out);
        assert!(s.online);
        assert_eq!(s.battery_level, Some(95));
        assert_eq!(s.estimated_minutes, Some(40));
        assert_eq!(s.model, "Smart-UPS 1500");
    }

    #[test]
    fn parse_upsc_on_battery_low() {
        let out = "\
ups.status: OB LB
battery.charge: 12
battery.runtime: 180
";
        let s = parse_upsc_output(out);
        assert!(!s.online);
        assert_eq!(s.battery_level, Some(12));
        assert_eq!(s.estimated_minutes, Some(3));
        assert_eq!(s.model, "unknown");
    }

    #[test]
    fn parse_upsc_missing_status_defaults_offline() {
        let s = parse_upsc_output("battery.charge: 50\n");
        assert!(!s.online);
    }

    // ---- sensors 解析 ----

    #[test]
    fn parse_sensors_basic() {
        let out = "\
coretemp-isa-0000
Adapter: ISA adapter
Package id 0:  +45.0°C  (high = +80.0°C)
temp1_input: 45.000
temp1_max: 80.000

it8728-isa-0a30
Adapter: ISA adapter
CPU_FAN: 1245 RPM
fan1_input: 1245
fan1_min: 0
";
        let (temps, fans) = parse_sensors_output(out);
        assert_eq!(temps.len(), 1);
        assert!((temps[0].celsius - 45.0).abs() < 0.01);
        assert_eq!(fans.len(), 1);
        assert_eq!(fans[0].rpm, 1245);
    }

    // ---- smartctl 解析 ----

    #[test]
    fn parse_smartctl_sata() {
        let json = r#"{
            "smart_status": {"passed": true},
            "temperature": {"current": 35},
            "ata_smart_attributes": {"table": [
                {"id": 5, "name": "Reallocated_Sector_Ct", "value": 100, "worst": 100, "thresh": 5, "raw": {"value": 0}},
                {"id": 9, "name": "Power_On_Hours", "value": 99, "worst": 99, "thresh": 0, "raw": {"value": 12000}},
                {"id": 197, "name": "Current_Pending_Sector", "value": 100, "worst": 100, "thresh": 0, "raw": {"value": 0}}
            ]}
        }"#;
        let (report, attrs) = parse_smartctl_json("/dev/sda", json).unwrap();
        assert!(report.passed);
        assert_eq!(report.disk, "/dev/sda");
        assert!((report.temperature - 35.0).abs() < 0.01);
        assert_eq!(report.reallocated_sectors, 0);
        assert_eq!(report.power_on_hours, 12000);
        assert_eq!(attrs.len(), 3);
        assert_eq!(assess_smart_health(&report, &attrs), SmartHealth::Healthy);
    }

    #[test]
    fn parse_smartctl_failed() {
        let json = r#"{
            "smart_status": {"passed": false},
            "temperature": {"current": 41},
            "ata_smart_attributes": {"table": [
                {"id": 5, "name": "Reallocated_Sector_Ct", "raw": {"value": 200}, "worst": 50, "thresh": 5}
            ]}
        }"#;
        let (report, attrs) = parse_smartctl_json("/dev/sdb", json).unwrap();
        assert!(!report.passed);
        assert_eq!(report.reallocated_sectors, 200);
        assert_eq!(assess_smart_health(&report, &attrs), SmartHealth::Failed);
    }

    #[test]
    fn parse_smartctl_degraded() {
        // passed=true，但 reallocated raw 在 (0, 100] 之间 → Degraded
        let json = r#"{
            "smart_status": {"passed": true},
            "temperature": {"current": 40},
            "ata_smart_attributes": {"table": [
                {"id": 5, "name": "Reallocated_Sector_Ct", "raw": {"value": 5}, "worst": 90, "thresh": 5}
            ]}
        }"#;
        let (report, attrs) = parse_smartctl_json("/dev/sdc", json).unwrap();
        assert_eq!(assess_smart_health(&report, &attrs), SmartHealth::Degraded);
    }

    #[test]
    fn parse_smartctl_bad_json() {
        let res = parse_smartctl_json("/dev/sda", "not json");
        assert!(res.is_err());
    }

    // ---- cron 校验 ----

    #[test]
    fn cron_valid() {
        assert!(is_valid_cron("0 3 * * *"));
        assert!(is_valid_cron("*/15 * * * *"));
        // 周字段支持 0-7（0 与 7 均表周日）；名称（mon-fri）暂不支持，须用数字。
        assert!(is_valid_cron("0 0 1 1,7 1-5"));
        assert!(is_valid_cron("30 22 * * 1-5"));
    }

    #[test]
    fn cron_invalid() {
        assert!(!is_valid_cron("0 3")); // 段数不足
        assert!(!is_valid_cron("60 0 * * *")); // 分钟越界
        assert!(!is_valid_cron("0 24 * * *")); // 小时越界
        assert!(!is_valid_cron("0 0 0 * *")); // 日下界（0 非法）
        assert!(!is_valid_cron("0 0 * 13 *")); // 月越界
    }

    // ---- LinuxPowerManager 行为（dry_run） ----

    #[tokio::test]
    async fn manager_dry_run_ups_errors() {
        let m = LinuxPowerManager::new("ups@localhost");
        let err = m.ups_status().await.unwrap_err();
        assert!(matches!(err, ServiceError::HardwareError(_)));
    }

    #[tokio::test]
    async fn manager_schedule_power_records_audit_and_state() {
        let m = LinuxPowerManager::new("ups@localhost");
        let sched = PowerSchedule {
            power_on_cron: Some("0 3 * * *".into()),
            shutdown_cron: Some("0 23 * * *".into()),
        };
        m.schedule_power(sched).await.unwrap();
        // PowerSchedule 未派生 PartialEq（契约层不改）；逐字段比较。
        let got = m.current_schedule().expect("schedule persisted");
        assert_eq!(got.power_on_cron.as_deref(), Some("0 3 * * *"));
        assert_eq!(got.shutdown_cron.as_deref(), Some("0 23 * * *"));
        let log = m.audit_log();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].action, "schedule_power");
    }

    #[tokio::test]
    async fn manager_schedule_power_rejects_bad_cron() {
        let m = LinuxPowerManager::new("ups@localhost");
        let sched = PowerSchedule {
            power_on_cron: Some("not a cron".into()),
            shutdown_cron: None,
        };
        let err = m.schedule_power(sched).await.unwrap_err();
        assert!(matches!(err, ServiceError::Internal(_)));
    }

    #[tokio::test]
    async fn manager_force_shutdown_logs_and_errors() {
        let m = LinuxPowerManager::new("ups@localhost");
        let err = m
            .force_shutdown("battery low: 8% / 3min")
            .await
            .unwrap_err();
        assert!(matches!(err, ServiceError::HardwareError(_)));
        let log = m.audit_log();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].action, "force_shutdown");
        assert_eq!(log[0].detail, "battery low: 8% / 3min");
    }

    #[tokio::test]
    async fn manager_smart_check_dry_run_errors() {
        let m = LinuxPowerManager::new("ups@localhost");
        let err = m.smart_check("/dev/sda").await.unwrap_err();
        assert!(matches!(err, ServiceError::HardwareError(_)));
    }

    // ---- cron 校验：补足 validate_cron_term 各分支 ----

    #[test]
    fn cron_range_with_step_valid() {
        // a-b/n —— validate_cron_term 的 range+step 分支
        assert!(is_valid_cron("0-59/15 * * * *"));
        assert!(is_valid_cron("0 0-23/2 * * *"));
    }

    #[test]
    fn cron_wildcard_with_step_various() {
        // */n 已覆盖；这里测 */1（最小步长）与字段表内含 *
        assert!(is_valid_cron("*/1 */1 * * *"));
        // 单独 * 在每个字段都应合法（覆盖 range_part=="*" 分支）
        assert!(is_valid_cron("* * * * *"));
    }

    #[test]
    fn cron_list_field_multiple_terms() {
        // 逗号列表：mix of single + range
        assert!(is_valid_cron("0,15,30,45 * * * *"));
        assert!(is_valid_cron("0,30 0,12 1,15 * *"));
    }

    #[test]
    fn cron_step_zero_or_non_numeric_rejected() {
        // */0 → 步长 0 非法
        assert!(!is_valid_cron("*/0 * * * *"));
        // 步长非数字
        assert!(!is_valid_cron("*/x * * * *"));
        // a-b/n 的 n=0
        assert!(!is_valid_cron("0-59/0 * * * *"));
        // a-b/n 的 n 非数字
        assert!(!is_valid_cron("0-59/x * * * *"));
    }

    #[test]
    fn cron_inverted_range_rejected() {
        // start > end → false（validate_cron_term 的 start>end 分支）
        assert!(!is_valid_cron("30-0 * * * *")); // 分钟倒序
        assert!(!is_valid_cron("0 12-0 * * *")); // 小时倒序
    }

    #[test]
    fn cron_non_numeric_single_value_rejected() {
        // 单值非数字 → false（range_part.parse 失败分支）
        assert!(!is_valid_cron("abc * * * *"));
    }

    #[test]
    fn cron_range_non_numeric_rejected() {
        // a-b 其中一边非数字 → false（split_once('-') 后 parse 失败分支）
        assert!(!is_valid_cron("a-b * * * *"));
        assert!(!is_valid_cron("0-z * * * *"));
    }

    #[test]
    fn cron_dow_field_accepts_zero_and_seven() {
        // 周字段 RANGES=(0,7)：0 与 7 均合法
        assert!(is_valid_cron("0 0 * * 0"));
        assert!(is_valid_cron("0 0 * * 7"));
        // 8 越界
        assert!(!is_valid_cron("0 0 * * 8"));
    }

    #[test]
    fn cron_month_field_boundary() {
        // 月 [1,12]：1 与 12 合法；0/13 非法
        assert!(is_valid_cron("0 0 * 1 *"));
        assert!(is_valid_cron("0 0 * 12 *"));
        assert!(!is_valid_cron("0 0 * 0 *"));
        assert!(!is_valid_cron("0 0 * 13 *"));
    }

    #[test]
    fn cron_too_many_or_too_few_fields() {
        assert!(!is_valid_cron("* * * *")); // 4 段
        assert!(!is_valid_cron("* * * * * *")); // 6 段
        assert!(!is_valid_cron("")); // 0 段
    }

    #[test]
    fn cron_empty_term_in_list_rejected() {
        // 逗号末尾空项 → 单值空串 parse 失败
        assert!(!is_valid_cron("0, * * * *"));
    }

    // ---- UPS 决策补足分支 ----

    #[test]
    fn ups_decision_any_mode_unknown_minutes_treated_dangerous() {
        // require_both=false + 续航未知：min_low=true(未知保守) → Shutdown
        let cfg = UpsShutdownConfig {
            require_both: false,
            ..Default::default()
        };
        // 电量充足（80）但续航未知 → any 模式触发
        assert_eq!(
            decide_ups_shutdown(&ups(false, Some(80), None), &cfg),
            UpsShutdownDecision::Shutdown {
                battery: Some(80),
                minutes: None
            }
        );
    }

    #[test]
    fn ups_decision_require_both_only_one_low_stays_safe() {
        // require_both=true + 仅电量低 + 续航充足 → Safe（min_low=false）
        let cfg = UpsShutdownConfig::default();
        assert_eq!(
            decide_ups_shutdown(&ups(false, Some(10), Some(60)), &cfg),
            UpsShutdownDecision::Safe
        );
    }

    #[test]
    fn ups_decision_require_both_both_unknown_triggers() {
        // 电量与续航都未知 → bat_low=true, min_low=true → Shutdown
        let cfg = UpsShutdownConfig::default();
        assert_eq!(
            decide_ups_shutdown(&ups(false, None, None), &cfg),
            UpsShutdownDecision::Shutdown {
                battery: None,
                minutes: None
            }
        );
    }

    #[test]
    fn ups_decision_any_mode_neither_low_stays_safe() {
        // require_both=false + 电量续航都充足 → Safe（bat_low=false, min_low=false）
        let cfg = UpsShutdownConfig {
            require_both: false,
            ..Default::default()
        };
        assert_eq!(
            decide_ups_shutdown(&ups(false, Some(80), Some(60)), &cfg),
            UpsShutdownDecision::Safe
        );
    }

    #[test]
    fn ups_decision_exact_threshold_stays_safe() {
        // 边界：battery == threshold（30）不算低（< 严格小于）
        let cfg = UpsShutdownConfig::default();
        assert_eq!(
            decide_ups_shutdown(&ups(false, Some(30), Some(10)), &cfg),
            UpsShutdownDecision::Safe
        );
        // minutes == threshold（10）不算低
        assert_eq!(
            decide_ups_shutdown(&ups(false, Some(20), Some(10)), &cfg),
            UpsShutdownDecision::Safe
        );
    }

    #[test]
    fn manager_shutdown_decision_wraps_default_config() {
        // shutdown_decision 用默认 cfg 包装——online 永不触发
        let m = LinuxPowerManager::new("ups@localhost");
        assert_eq!(
            m.shutdown_decision(&ups(true, Some(0), Some(0))),
            UpsShutdownDecision::Online
        );
        // 离线 + 电量低 + 续航低 → Shutdown
        assert!(matches!(
            m.shutdown_decision(&ups(false, Some(5), Some(3))),
            UpsShutdownDecision::Shutdown { .. }
        ));
    }

    // ---- parse_upsc_output 边界 ----

    #[test]
    fn parse_upsc_battery_over_100_clamped() {
        // 电量 > 100 应被钳到 100（.min(100)）
        let out = "ups.status: OL\nbattery.charge: 150\n";
        let s = parse_upsc_output(out);
        assert_eq!(s.battery_level, Some(100));
    }

    #[test]
    fn parse_upsc_invalid_battery_value_ignored() {
        // 非数字电量 → None
        let out = "ups.status: OL\nbattery.charge: abc\n";
        let s = parse_upsc_output(out);
        assert_eq!(s.battery_level, None);
    }

    #[test]
    fn parse_upsc_runtime_sub_minute_rounds_to_zero() {
        // 续航 < 60 秒 → minutes=0（整除）
        let out = "ups.status: OB\nbattery.runtime: 30\n";
        let s = parse_upsc_output(out);
        assert_eq!(s.estimated_minutes, Some(0));
    }

    #[test]
    fn parse_upsc_device_model_fallback() {
        // device.model 作为 ups.model 的回退
        let out = "ups.status: OL\ndevice.model: BackUPS\n";
        let s = parse_upsc_output(out);
        assert_eq!(s.model, "BackUPS");
    }

    #[test]
    fn parse_upsc_empty_input_defaults_offline() {
        // 完全空输入 → 离线（保守默认）
        let s = parse_upsc_output("");
        assert!(!s.online);
        assert_eq!(s.model, "unknown");
    }

    #[test]
    fn parse_upsc_line_without_colon_ignored() {
        // 无冒号行应被忽略（不 panic）
        let s = parse_upsc_output("garbage line\nups.status: OL\n");
        assert!(s.online);
    }

    #[test]
    fn parse_upsc_runtime_invalid_ignored() {
        let s = parse_upsc_output("ups.status: OL\nbattery.runtime: notnum\n");
        assert_eq!(s.estimated_minutes, None);
    }

    // ---- parse_sensors_output 边界 ----

    #[test]
    fn parse_sensors_invalid_values_skipped() {
        // 数值非法的 input 行应被忽略（不 panic、不入结果）
        let out = "\
block
temp1_input: notanumber
temp2_input: 25.5
fan1_input: notanumber
fan2_input: 1500
";
        let (temps, fans) = parse_sensors_output(out);
        assert_eq!(temps.len(), 1);
        assert!((temps[0].celsius - 25.5).abs() < 0.01);
        assert_eq!(fans.len(), 1);
        assert_eq!(fans[0].rpm, 1500);
    }

    #[test]
    fn parse_sensors_empty_input() {
        let (temps, fans) = parse_sensors_output("");
        assert!(temps.is_empty());
        assert!(fans.is_empty());
    }

    #[test]
    fn parse_sensors_no_label_uses_indexed_default() {
        // 无描述行标签 → 用 tempN / fanN 默认名
        let out = "temp1_input: 40.0\nfan1_input: 1000\n";
        let (temps, fans) = parse_sensors_output(out);
        assert_eq!(temps.len(), 1);
        assert_eq!(temps[0].label, "temp1");
        assert_eq!(fans.len(), 1);
        assert_eq!(fans[0].label, "fan1");
    }

    // ---- parse_smartctl_json 边界 ----

    #[test]
    fn parse_smartctl_empty_attrs_table_ok() {
        // 空 table 数组 → 属性表空，reallocated/power_on 为 0
        let json = r#"{
            "smart_status": {"passed": true},
            "temperature": {"current": 30},
            "ata_smart_attributes": {"table": []}
        }"#;
        let (report, attrs) = parse_smartctl_json("/dev/sda", json).unwrap();
        assert!(report.passed);
        assert_eq!(attrs.len(), 0);
        assert_eq!(report.reallocated_sectors, 0);
        assert_eq!(report.power_on_hours, 0);
    }

    #[test]
    fn parse_smartctl_missing_smart_status_defaults_failed() {
        // 无 smart_status → passed=false（保守）
        let json = r#"{"temperature": {"current": 30}}"#;
        let (report, _) = parse_smartctl_json("/dev/sda", json).unwrap();
        assert!(!report.passed);
    }

    #[test]
    fn parse_smartctl_missing_temperature_defaults_zero() {
        let json = r#"{"smart_status": {"passed": true}}"#;
        let (report, _) = parse_smartctl_json("/dev/sda", json).unwrap();
        assert!((report.temperature - 0.0).abs() < 0.01);
    }

    #[test]
    fn parse_smartctl_nvme_no_attrs_table() {
        // NVMe 盘无 ata_smart_attributes → 属性表空
        let json = r#"{
            "smart_status": {"passed": true},
            "temperature": {"current": 35}
        }"#;
        let (report, attrs) = parse_smartctl_json("/dev/nvme0", json).unwrap();
        assert_eq!(attrs.len(), 0);
        assert_eq!(report.disk, "/dev/nvme0");
    }

    #[test]
    fn parse_smartctl_attr_missing_raw_value_defaults_zero() {
        // 属性 row 缺 raw.value → 0
        let json = r#"{
            "smart_status": {"passed": true},
            "ata_smart_attributes": {"table": [
                {"id": 5, "name": "Reallocated_Sector_Ct"}
            ]}
        }"#;
        let (report, attrs) = parse_smartctl_json("/dev/sda", json).unwrap();
        assert_eq!(attrs.len(), 1);
        assert_eq!(attrs[0].raw, 0);
        assert_eq!(report.reallocated_sectors, 0);
    }

    #[test]
    fn assess_smart_health_failed_limit_boundary() {
        // 关键属性 raw > FAILED_RAW_LIMIT(100) 才 Failed；raw==100 仍 Degraded（> 严格大于）
        let report_ok = SmartReport {
            disk: "/dev/sda".into(),
            passed: true,
            temperature: 30.0,
            reallocated_sectors: 0,
            power_on_hours: 0,
        };
        // raw=101 → Failed
        let attrs_failed = vec![SmartAttribute {
            id: 5,
            name: "Reallocated".into(),
            raw: 101,
            worst: 50,
            threshold: 5,
        }];
        assert_eq!(
            assess_smart_health(&report_ok, &attrs_failed),
            SmartHealth::Failed
        );
        // raw=100（恰好等于阈值，> 100 为 false）→ Degraded
        let attrs_boundary = vec![SmartAttribute {
            id: 5,
            name: "Reallocated".into(),
            raw: 100,
            worst: 50,
            threshold: 5,
        }];
        assert_eq!(
            assess_smart_health(&report_ok, &attrs_boundary),
            SmartHealth::Degraded
        );
        // raw=99 → Degraded
        let attrs_degraded = vec![SmartAttribute {
            id: 197,
            name: "Pending".into(),
            raw: 99,
            worst: 50,
            threshold: 5,
        }];
        assert_eq!(
            assess_smart_health(&report_ok, &attrs_degraded),
            SmartHealth::Degraded
        );
    }

    #[test]
    fn assess_smart_health_offline_uncorrectable_triggers() {
        // id=198 (Offline_Uncorrectable) raw>0 → Degraded
        let report = SmartReport {
            disk: "/dev/sda".into(),
            passed: true,
            temperature: 30.0,
            reallocated_sectors: 0,
            power_on_hours: 0,
        };
        let attrs = vec![SmartAttribute {
            id: 198,
            name: "Offline_Uncorrectable".into(),
            raw: 1,
            worst: 50,
            threshold: 5,
        }];
        assert_eq!(assess_smart_health(&report, &attrs), SmartHealth::Degraded);
    }

    #[test]
    fn assess_smart_health_healthy_when_critical_all_zero() {
        // 关键属性全 0 → Healthy
        let report = SmartReport {
            disk: "/dev/sda".into(),
            passed: true,
            temperature: 30.0,
            reallocated_sectors: 0,
            power_on_hours: 0,
        };
        let attrs = vec![
            SmartAttribute {
                id: 5,
                name: "Reallocated".into(),
                raw: 0,
                worst: 100,
                threshold: 5,
            },
            SmartAttribute {
                id: 197,
                name: "Pending".into(),
                raw: 0,
                worst: 100,
                threshold: 0,
            },
            SmartAttribute {
                id: 198,
                name: "Offline".into(),
                raw: 0,
                worst: 100,
                threshold: 0,
            },
        ];
        assert_eq!(assess_smart_health(&report, &attrs), SmartHealth::Healthy);
    }

    #[test]
    fn assess_smart_health_non_critical_attrs_ignored() {
        // 非 critical id（如 9 power_on_hours）raw>0 不影响 Healthy
        let report = SmartReport {
            disk: "/dev/sda".into(),
            passed: true,
            temperature: 30.0,
            reallocated_sectors: 0,
            power_on_hours: 999,
        };
        let attrs = vec![SmartAttribute {
            id: 9,
            name: "Power_On_Hours".into(),
            raw: 999,
            worst: 99,
            threshold: 0,
        }];
        assert_eq!(assess_smart_health(&report, &attrs), SmartHealth::Healthy);
    }

    // ---- LinuxPowerManager ring buffer + read_temps/fans dry_run ----

    #[tokio::test]
    async fn manager_read_temps_and_fans_dry_run_errors() {
        let m = LinuxPowerManager::new("ups@localhost");
        let err = m.read_temps().await.unwrap_err();
        assert!(matches!(err, ServiceError::HardwareError(_)));
        let err = m.read_fans().await.unwrap_err();
        assert!(matches!(err, ServiceError::HardwareError(_)));
    }

    #[tokio::test]
    async fn manager_audit_ring_buffer_evicts_oldest() {
        // ring buffer 容量 64：连续 force_shutdown 65 次应保留最近 64 条
        let m = LinuxPowerManager::new("ups@localhost");
        for i in 0..65 {
            // force_shutdown 始终返回错误但记录审计
            let reason = format!("reason-{i}");
            let _ = m.force_shutdown(&reason).await;
        }
        let log = m.audit_log();
        assert_eq!(log.len(), 64, "ring buffer 应截断到 64 条");
        // 最早一条应是 reason-1（reason-0 被驱逐）
        assert_eq!(log[0].detail, "reason-1");
        assert_eq!(log[63].detail, "reason-64");
    }

    #[tokio::test]
    async fn manager_schedule_power_shutdown_cron_invalid_rejected() {
        // shutdown_cron 非法 → Internal 错误
        let m = LinuxPowerManager::new("ups@localhost");
        let sched = PowerSchedule {
            power_on_cron: None,
            shutdown_cron: Some("99 99 * * *".into()),
        };
        let err = m.schedule_power(sched).await.unwrap_err();
        assert!(matches!(err, ServiceError::Internal(_)));
        // schedule 未持久化
        assert!(m.current_schedule().is_none());
    }

    #[tokio::test]
    async fn manager_schedule_power_both_none_ok() {
        // 两条 cron 都 None → 合法（不做校验），仍记审计 + 持久化
        let m = LinuxPowerManager::new("ups@localhost");
        let sched = PowerSchedule {
            power_on_cron: None,
            shutdown_cron: None,
        };
        m.schedule_power(sched).await.unwrap();
        assert!(m.current_schedule().is_some());
        assert_eq!(m.audit_log().len(), 1);
    }
}
