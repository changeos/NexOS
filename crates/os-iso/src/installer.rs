//! Rust 安装器（规划文档 §3.11 / §10.2#17 HCL）
//!
//! 职责：硬件兼容性检测（HCL）+ 实际安装（分区/建池/装系统/首启动作）。
//! 安装期首启强制重设密码（呼应 §3.19）。

use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// 安装步骤状态机（裸机安装的有序阶段）
// ----------------------------------------------------------------------------

/// 安装步骤（裸机安装的有序阶段，状态机推进）。
///
/// 顺序（`next`）：
/// `Partition` → `CreatePool` → `ExtractRootfs` → `InstallComponents`
///   → `ConfigureSystem` → `SetupFirstBoot` → `Done`
///
/// 任一步失败即终止（`Done` 不能由失败态推进）。真实裸机执行（分区/建池/写盘）留
/// `RustInstaller::install` 内 TODO（需沙箱 + 嵌套虚拟化）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallStep {
    /// 分区（写磁盘分区表，高危！需 root）
    #[default]
    Partition,
    /// 创建 ZFS 池（zpool create，高危！）
    CreatePool,
    /// 解压 rootfs（squashfs → 目标盘根文件系统）
    ExtractRootfs,
    /// 安装组件二进制（注入 osd / os-storage / ... 到目标盘）
    InstallComponents,
    /// 配置系统（网络 / locale / fstab / 用户）
    ConfigureSystem,
    /// 设置首启动作（首启强制重设 root 密码，§3.19）
    SetupFirstBoot,
    /// 安装完成
    Done,
}

impl InstallStep {
    /// 取下一步（`Done` 已为终态，返回自身；失败态由调用方另行记录）。
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Partition => Self::CreatePool,
            Self::CreatePool => Self::ExtractRootfs,
            Self::ExtractRootfs => Self::InstallComponents,
            Self::InstallComponents => Self::ConfigureSystem,
            Self::ConfigureSystem => Self::SetupFirstBoot,
            Self::SetupFirstBoot => Self::Done,
            Self::Done => Self::Done,
        }
    }

    /// 是否终态。
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done)
    }

    /// 取全部有序步骤（用于枚举校验/进度展示）。
    pub fn all_steps() -> &'static [InstallStep] {
        &[
            Self::Partition,
            Self::CreatePool,
            Self::ExtractRootfs,
            Self::InstallComponents,
            Self::ConfigureSystem,
            Self::SetupFirstBoot,
            Self::Done,
        ]
    }

    /// 步骤的人类可读中文名（用于日志/状态展示）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Partition => "分区",
            Self::CreatePool => "创建存储池",
            Self::ExtractRootfs => "解压根文件系统",
            Self::InstallComponents => "安装组件",
            Self::ConfigureSystem => "配置系统",
            Self::SetupFirstBoot => "设置首启",
            Self::Done => "完成",
        }
    }
}

// ----------------------------------------------------------------------------
// 安装目标
// ----------------------------------------------------------------------------

/// 安装目标（用户在安装器中选择的部署参数）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallTarget {
    /// 安装目标盘列表（设备路径）
    pub disks: Vec<String>,
    /// ZFS RAID 级别（None = 单盘 stripe；如 `mirror` / `raidz1` / `raidz2`）
    pub zfs_raid_level: Option<String>,
    /// root 密码哈希（首启强制重设——绝不预置明文）
    pub root_password_hash: String,
    /// 初始管理员用户名
    pub admin_user: String,
    /// 网络配置（开放结构，由安装器写入）
    pub network: serde_json::Value,
    /// 区域（如 `zh_CN.UTF-8`）
    pub locale: String,
}

impl InstallTarget {
    /// 校验安装目标合法（盘列表非空、root 密码哈希非空、admin_user 非空、locale 非空）。
    ///
    /// 注：不校验密码哈希强度（属业务层，由调用方决定）；不校验盘存在（属运行期探针）。
    pub fn validate(&self) -> Result<(), crate::IsoError> {
        if self.disks.is_empty() {
            return Err(crate::IsoError::InstallFailed(
                "至少需指定一块目标盘".to_string(),
            ));
        }
        for d in &self.disks {
            if d.trim().is_empty() {
                return Err(crate::IsoError::InstallFailed(
                    "目标盘路径不能为空".to_string(),
                ));
            }
        }
        if self.root_password_hash.trim().is_empty() {
            return Err(crate::IsoError::InstallFailed(
                "root_password_hash 不能为空（绝不预置明文，但哈希必填）".to_string(),
            ));
        }
        if self.admin_user.trim().is_empty() {
            return Err(crate::IsoError::InstallFailed(
                "admin_user 不能为空".to_string(),
            ));
        }
        if self.locale.trim().is_empty() {
            return Err(crate::IsoError::InstallFailed(
                "locale 不能为空".to_string(),
            ));
        }
        if let Some(level) = &self.zfs_raid_level {
            match level.as_str() {
                "stripe" | "mirror" | "raidz1" | "raidz2" | "raidz3" => {}
                other => {
                    return Err(crate::IsoError::InstallFailed(format!(
                        "不支持的 ZFS RAID 级别：{other}"
                    )));
                }
            }
        }
        Ok(())
    }

    /// 校验 RAID 级别与盘数兼容（如 mirror 至少 2 盘，raidz1 至少 3 盘）。
    pub fn validate_raid_disk_count(&self) -> Result<(), crate::IsoError> {
        let n = self.disks.len();
        match self.zfs_raid_level.as_deref() {
            None | Some("stripe") => {
                if n < 1 {
                    return Err(crate::IsoError::InstallFailed(
                        "stripe/单盘至少需 1 块盘".to_string(),
                    ));
                }
            }
            Some("mirror") => {
                if n < 2 {
                    return Err(crate::IsoError::InstallFailed(
                        "mirror 至少需 2 块盘".to_string(),
                    ));
                }
            }
            Some("raidz1") => {
                if n < 3 {
                    return Err(crate::IsoError::InstallFailed(
                        "raidz1 至少需 3 块盘".to_string(),
                    ));
                }
            }
            Some("raidz2") => {
                if n < 4 {
                    return Err(crate::IsoError::InstallFailed(
                        "raidz2 至少需 4 块盘".to_string(),
                    ));
                }
            }
            Some("raidz3") => {
                if n < 5 {
                    return Err(crate::IsoError::InstallFailed(
                        "raidz3 至少需 5 块盘".to_string(),
                    ));
                }
            }
            Some(other) => {
                return Err(crate::IsoError::InstallFailed(format!(
                    "不支持的 ZFS RAID 级别：{other}"
                )));
            }
        }
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// 硬件报告
// ----------------------------------------------------------------------------

/// 单块磁盘信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiskInfo {
    /// 设备路径（如 `/dev/sda`）
    pub device: String,
    /// 容量（GB）
    pub size_gb: u64,
    /// 型号（如 `Samsung SSD 870`）
    pub model: String,
    /// 是否机械盘（true = HDD；false = SSD/NVMe）
    pub rotational: bool,
}

/// 硬件兼容性报告（呼应 §10.2#17 HCL）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HardwareReport {
    /// CPU 型号
    pub cpu: String,
    /// 内存容量（GB）
    pub memory_gb: u64,
    /// 磁盘列表
    pub disks: Vec<DiskInfo>,
    /// 网卡列表（接口名）
    pub nics: Vec<String>,
    /// 是否支持 KVM 硬件虚拟化
    pub kvm_support: bool,
    /// 兼容性告警（如 `["内存低于推荐 8GB","NIC x 的驱动为闭源"]`）
    pub warnings: Vec<String>,
}

// ----------------------------------------------------------------------------
// 安装报告
// ----------------------------------------------------------------------------

/// 安装结果报告
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallReport {
    /// 已安装组件列表（如 `["osd","os-storage","os-meta"]`）
    pub installed_components: Vec<String>,
    /// 创建的 ZFS 池名（如 `tank`；None = 未建池）
    pub pool_created: Option<String>,
    /// 安装耗时（秒）
    pub duration_secs: u64,
    /// 首启待执行动作（如 `["首次登录强制重设 root 密码","初始化管理员"]`）
    pub post_install_actions: Vec<String>,
    /// 安装期构造的全部命令列表（`程序名, 参数`），便于审计 / 沙箱回放。
    ///
    /// 注：当前实现由 [`crate::install_cmds`] 纯函数构造，**未真实执行**（真实 spawn
    /// 留 runner 注入）。下游可据此 dry-run 审查将执行的命令而不写盘。
    #[serde(default)]
    pub commands: Vec<(String, Vec<String>)>,
}

// ----------------------------------------------------------------------------
// Installer trait（async）
// ----------------------------------------------------------------------------

/// Rust 安装器——硬件检测 + 实际安装。
///
/// 实现者：`RustInstaller`（默认）。安装为长任务，结果一次性返回。
#[allow(async_fn_in_trait)]
pub trait Installer: Send + Sync {
    /// 硬件兼容性检测（HCL），返回报告与告警。
    async fn detect_hardware(&self) -> Result<HardwareReport, crate::IsoError>;

    /// 执行安装：从给定 ISO 装到 `target` 指定的盘/池配置。
    async fn install(
        &self,
        iso_path: &std::path::Path,
        target: InstallTarget,
    ) -> Result<InstallReport, crate::IsoError>;
}

// ----------------------------------------------------------------------------
// HCL（硬件兼容性清单）阈值与 KVM 检测（纯函数，可单测）
// ----------------------------------------------------------------------------

/// HCL 推荐阈值（呼应规格书 §10.2#17）。
#[derive(Debug, Clone, Copy)]
pub struct HclThresholds {
    /// 最低内存（GB）——低于此值告警。
    pub min_memory_gb: u64,
    /// 推荐内存（GB）——低于此值告警。
    pub recommended_memory_gb: u64,
    /// 最低盘容量（GB）。
    pub min_disk_gb: u64,
}

impl Default for HclThresholds {
    fn default() -> Self {
        // 与主文档 §10.2#17 推荐配置对齐
        Self {
            min_memory_gb: 4,
            recommended_memory_gb: 8,
            min_disk_gb: 32,
        }
    }
}

/// 由 `/proc/cpuinfo` 文本判定是否支持 KVM 硬件虚拟化（Intel vmx 或 AMD svm flag）。
///
/// 纯函数：调用方传入 cpuinfo 文本（便于注入 fixture 测），不直接读文件。
/// 命中 `flags` 行包含 `vmx`（Intel VT-x）或 `svm`（AMD-V）即视为支持。
pub fn detect_kvm_support_from_cpuinfo(cpuinfo: &str) -> bool {
    for line in cpuinfo.lines() {
        if let Some(rest) = line.strip_prefix("flags") {
            // flags\t: ... vmx ... svm ...
            let flags = rest.trim_start_matches([' ', '\t', ':']);
            for tok in flags.split_whitespace() {
                if tok == "vmx" || tok == "svm" {
                    return true;
                }
            }
        }
    }
    false
}

/// 对 `HardwareReport` 按 HCL 阈值生成告警（不修改输入，返回新告警列表）。
///
/// 检查项：内存低于推荐、最低盘容量、无盘、无网卡、无 KVM 支持。
pub fn hcl_warnings(report: &HardwareReport, thresholds: &HclThresholds) -> Vec<String> {
    let mut warns = Vec::new();
    if report.memory_gb < thresholds.min_memory_gb {
        warns.push(format!(
            "内存 {}GB 低于最低要求 {}GB",
            report.memory_gb, thresholds.min_memory_gb
        ));
    } else if report.memory_gb < thresholds.recommended_memory_gb {
        warns.push(format!(
            "内存 {}GB 低于推荐 {}GB",
            report.memory_gb, thresholds.recommended_memory_gb
        ));
    }
    if report.disks.is_empty() {
        warns.push("未检测到任何磁盘".to_string());
    } else {
        let mut too_small = 0;
        for d in &report.disks {
            if d.size_gb < thresholds.min_disk_gb {
                too_small += 1;
            }
        }
        if too_small > 0 {
            warns.push(format!(
                "{too_small} 块盘容量低于最低 {}GB",
                thresholds.min_disk_gb
            ));
        }
    }
    if report.nics.is_empty() {
        warns.push("未检测到网卡".to_string());
    }
    if !report.kvm_support {
        warns.push("CPU 不支持 KVM 硬件虚拟化（虚拟机/容器性能受限）".to_string());
    }
    warns
}

// ----------------------------------------------------------------------------
// 单元测试（InstallStep 状态机、InstallTarget 校验、HCL、KVM 检测）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid_target() -> InstallTarget {
        InstallTarget {
            disks: vec!["/dev/sda".to_string()],
            zfs_raid_level: None,
            root_password_hash: "$6$rounds=...$hash".to_string(),
            admin_user: "admin".to_string(),
            network: json!({"mode": "dhcp"}),
            locale: "zh_CN.UTF-8".to_string(),
        }
    }

    // —— InstallStep 状态机 ——

    #[test]
    fn step_sequence_full() {
        let mut step = InstallStep::Partition;
        let expected = [
            InstallStep::CreatePool,
            InstallStep::ExtractRootfs,
            InstallStep::InstallComponents,
            InstallStep::ConfigureSystem,
            InstallStep::SetupFirstBoot,
            InstallStep::Done,
        ];
        for e in expected {
            step = step.next();
            assert_eq!(step, e);
        }
        // 终态保持
        assert!(step.is_terminal());
        assert_eq!(step.next(), InstallStep::Done);
    }

    #[test]
    fn step_default_is_partition() {
        assert_eq!(InstallStep::default(), InstallStep::Partition);
    }

    #[test]
    fn step_all_steps_complete() {
        let all = InstallStep::all_steps();
        assert_eq!(all.len(), 7);
        assert_eq!(all[0], InstallStep::Partition);
        assert_eq!(all[6], InstallStep::Done);
        // 序列应是 next 推进的链
        for w in all.windows(2) {
            assert_eq!(w[0].next(), w[1]);
        }
    }

    #[test]
    fn step_labels_nonempty_distinct() {
        let labels: Vec<&str> = InstallStep::all_steps().iter().map(|s| s.label()).collect();
        for l in &labels {
            assert!(!l.is_empty());
        }
        // 各步标签互异
        let mut sorted = labels.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "标签应互异: {labels:?}");
    }

    #[test]
    fn step_is_terminal() {
        assert!(InstallStep::Done.is_terminal());
        assert!(!InstallStep::Partition.is_terminal());
        assert!(!InstallStep::CreatePool.is_terminal());
    }

    // —— InstallTarget::validate ——

    #[test]
    fn target_validate_ok() {
        assert!(valid_target().validate().is_ok());
    }

    #[test]
    fn target_validate_empty_disks() {
        let mut t = valid_target();
        t.disks.clear();
        assert!(t.validate().unwrap_err().to_string().contains("目标盘"));
    }

    #[test]
    fn target_validate_blank_disk_path() {
        let mut t = valid_target();
        t.disks.push("  ".to_string());
        assert!(t.validate().is_err());
    }

    #[test]
    fn target_validate_empty_password_hash() {
        let mut t = valid_target();
        t.root_password_hash = String::new();
        let err = t.validate().unwrap_err();
        assert!(err.to_string().contains("root_password_hash"));
    }

    #[test]
    fn target_validate_empty_admin_user() {
        let mut t = valid_target();
        t.admin_user = String::new();
        assert!(t.validate().is_err());
    }

    #[test]
    fn target_validate_empty_locale() {
        let mut t = valid_target();
        t.locale = "  ".to_string();
        assert!(t.validate().is_err());
    }

    #[test]
    fn target_validate_bad_raid_level() {
        let mut t = valid_target();
        t.zfs_raid_level = Some("raidz9".to_string());
        assert!(t.validate().is_err());
    }

    #[test]
    fn target_validate_good_raid_levels() {
        for lvl in ["stripe", "mirror", "raidz1", "raidz2", "raidz3"] {
            let mut t = valid_target();
            t.zfs_raid_level = Some(lvl.to_string());
            assert!(t.validate().is_ok(), "level={lvl} 应合法");
        }
    }

    #[test]
    fn target_validate_none_raid_ok() {
        let t = valid_target();
        assert!(t.validate().is_ok());
    }

    // —— InstallTarget::validate_raid_disk_count ——

    #[test]
    fn raid_count_stripe_ok() {
        let mut t = valid_target();
        t.zfs_raid_level = None;
        assert!(t.validate_raid_disk_count().is_ok());
        t.zfs_raid_level = Some("stripe".to_string());
        assert!(t.validate_raid_disk_count().is_ok());
    }

    #[test]
    fn raid_count_mirror() {
        let mut t = valid_target();
        t.zfs_raid_level = Some("mirror".to_string());
        t.disks = vec!["/dev/sda".to_string()];
        assert!(t.validate_raid_disk_count().is_err());
        t.disks = vec!["/dev/sda".to_string(), "/dev/sdb".to_string()];
        assert!(t.validate_raid_disk_count().is_ok());
    }

    #[test]
    fn raid_count_raidz1() {
        let mut t = valid_target();
        t.zfs_raid_level = Some("raidz1".to_string());
        t.disks = vec!["a".into(), "b".into()];
        assert!(t.validate_raid_disk_count().is_err());
        t.disks.push("c".into());
        assert!(t.validate_raid_disk_count().is_ok());
    }

    #[test]
    fn raid_count_raidz2() {
        let mut t = valid_target();
        t.zfs_raid_level = Some("raidz2".to_string());
        t.disks = vec!["a".into(), "b".into(), "c".into()];
        assert!(t.validate_raid_disk_count().is_err());
        t.disks.push("d".into());
        assert!(t.validate_raid_disk_count().is_ok());
    }

    #[test]
    fn raid_count_raidz3() {
        let mut t = valid_target();
        t.zfs_raid_level = Some("raidz3".to_string());
        t.disks = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        assert!(t.validate_raid_disk_count().is_err());
        t.disks.push("e".into());
        assert!(t.validate_raid_disk_count().is_ok());
    }

    // —— KVM 检测 ——

    #[test]
    fn kvm_intel_vmx() {
        let cpuinfo = "processor\t: 0\nvendor_id\t: GenuineIntel\nflags\t: fpu vme de vmx msr\n";
        assert!(detect_kvm_support_from_cpuinfo(cpuinfo));
    }

    #[test]
    fn kvm_amd_svm() {
        let cpuinfo = "flags\t: fpu svm sep msr\n";
        assert!(detect_kvm_support_from_cpuinfo(cpuinfo));
    }

    #[test]
    fn kvm_none() {
        assert!(!detect_kvm_support_from_cpuinfo(
            "flags\t: fpu vme de msr\n"
        ));
        assert!(!detect_kvm_support_from_cpuinfo(""));
    }

    #[test]
    fn kvm_multiple_cpus_first_has_flag() {
        let cpuinfo = "processor\t: 0\nflags\t: fpu vmx msr\nprocessor\t: 1\nflags\t: fpu msr\n";
        assert!(detect_kvm_support_from_cpuinfo(cpuinfo));
    }

    #[test]
    fn kvm_substring_not_match() {
        // "vmxfoo" 不应命中（要求 token 精确匹配）
        assert!(!detect_kvm_support_from_cpuinfo("flags\t: vmxfoo svmbar\n"));
    }

    // —— HCL 告警 ——

    fn hw(memory_gb: u64, disks: Vec<u64>, nics: usize, kvm: bool) -> HardwareReport {
        HardwareReport {
            cpu: "x".to_string(),
            memory_gb,
            disks: disks
                .iter()
                .map(|gb| DiskInfo {
                    device: format!("/dev/d{gb}"),
                    size_gb: *gb,
                    model: "x".into(),
                    rotational: false,
                })
                .collect(),
            nics: (0..nics).map(|i| format!("eth{i}")).collect(),
            kvm_support: kvm,
            warnings: vec![],
        }
    }

    #[test]
    fn hcl_below_min_memory() {
        let r = hw(2, vec![500], 1, true);
        let w = hcl_warnings(&r, &HclThresholds::default());
        assert!(w.iter().any(|s| s.contains("低于最低要求")));
    }

    #[test]
    fn hcl_below_recommended_memory() {
        let r = hw(6, vec![500], 1, true);
        let w = hcl_warnings(&r, &HclThresholds::default());
        assert!(w.iter().any(|s| s.contains("低于推荐")));
        assert!(!w.iter().any(|s| s.contains("低于最低要求")));
    }

    #[test]
    fn hcl_no_disk() {
        let r = hw(16, vec![], 1, true);
        let w = hcl_warnings(&r, &HclThresholds::default());
        assert!(w.iter().any(|s| s.contains("磁盘")));
    }

    #[test]
    fn hcl_small_disk() {
        let r = hw(16, vec![16], 1, true);
        let w = hcl_warnings(&r, &HclThresholds::default());
        assert!(w.iter().any(|s| s.contains("容量低于")));
    }

    #[test]
    fn hcl_no_nic() {
        let r = hw(16, vec![500], 0, true);
        let w = hcl_warnings(&r, &HclThresholds::default());
        assert!(w.iter().any(|s| s.contains("网卡")));
    }

    #[test]
    fn hcl_no_kvm() {
        let r = hw(16, vec![500], 1, false);
        let w = hcl_warnings(&r, &HclThresholds::default());
        assert!(w.iter().any(|s| s.contains("KVM")));
    }

    #[test]
    fn hcl_all_good_no_warnings() {
        let r = hw(32, vec![1000], 1, true);
        let w = hcl_warnings(&r, &HclThresholds::default());
        assert!(w.is_empty(), "应无告警: {w:?}");
    }

    #[test]
    fn hcl_custom_thresholds() {
        let r = hw(8, vec![500], 1, true);
        let th = HclThresholds {
            min_memory_gb: 16,
            recommended_memory_gb: 32,
            min_disk_gb: 1000,
        };
        let w = hcl_warnings(&r, &th);
        assert!(w.iter().any(|s| s.contains("低于最低要求 16")));
        assert!(w.iter().any(|s| s.contains("容量低于最低 1000")));
    }

    #[test]
    fn hcl_multiple_small_disks_counted() {
        let r = hw(16, vec![16, 16, 500], 1, true);
        let w = hcl_warnings(&r, &HclThresholds::default());
        assert!(w.iter().any(|s| s.contains("2 块盘容量低于")));
    }
}
