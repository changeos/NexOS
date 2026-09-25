//! `RustInstaller` —— 硬件检测 + 裸机安装实现。
//!
//! 设计（呼应规格书 §3 / §10.2#17）：
//! - `detect_hardware`：探测 CPU/内存/磁盘/网卡/KVM 支持，返回 `HardwareReport`（含告警）。
//!   KVM flag 检测用纯函数 [`crate::installer::detect_kvm_support_from_cpuinfo`]，
//!   真实读 `/proc/cpuinfo`/`lsblk`/`/proc/meminfo` 留 TODO（沙箱 / 嵌套虚拟化）。
//! - `install`：按 [`InstallStep`] 状态机推进，分区 → 建池 → 解 rootfs → 装组件 →
//!   配置系统 → 首启重设密码 → Done。**真实裸机写盘留 TODO**（高危，沙箱）。
//!
//! 可测部分：状态机推进（纯函数）、HCL 告警（纯函数）、KVM 检测（纯函数）。
//! 不可测部分：真实硬件探测、真实分区/建池（需裸机）。

use crate::install_cmds::{
    configure_system_cmd, create_pool_cmd, extract_rootfs_cmd, install_bootloader_cmd,
    install_components_cmd, partition_cmd,
};
use crate::installer::{
    detect_kvm_support_from_cpuinfo, hcl_warnings, DiskInfo, HardwareReport, HclThresholds,
    InstallReport, InstallStep, InstallTarget, Installer,
};
use crate::IsoError;
use std::path::Path;
use std::time::Instant;
use tracing::{info, warn};

/// Rust 裸机安装器。
///
/// 真实硬件探测与写盘操作（分区/建池/装系统）留 TODO（需沙箱 + root + 嵌套虚拟化）。
/// 当前 `detect_hardware`/`install` 返回确定性占位结果，纯逻辑分支（校验、状态机、
/// HCL 告警）已可单测。
pub struct RustInstaller {
    /// HCL 阈值（影响 `detect_hardware` 的告警生成）。
    thresholds: HclThresholds,
}

impl Default for RustInstaller {
    fn default() -> Self {
        Self::new()
    }
}

impl RustInstaller {
    /// 构造（默认 HCL 阈值）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            thresholds: HclThresholds::default(),
        }
    }

    /// 自定义 HCL 阈值。
    #[must_use]
    pub fn with_thresholds(thresholds: HclThresholds) -> Self {
        Self { thresholds }
    }

    /// （TODO）真实读 `/proc/cpuinfo`。当前返回空串（KVM 检测将判 false）。
    ///
    /// 真实现应 `tokio::fs::read_to_string("/proc/cpuinfo").await`。
    fn read_cpuinfo_sync(&self) -> String {
        // TODO(裸机): tokio::fs::read_to_string("/proc/cpuinfo").await
        // 占位：返回空（不报错，KVM 判 false 并产生告警）
        String::new()
    }

    /// 构造占位 `HardwareReport`（真实探测留 TODO）。
    ///
    /// 占位值：CPU=unknown，内存=0，无盘/无网卡，KVM=false。调用方可在沙箱/真实环境
    /// 覆盖探测逻辑。
    fn placeholder_report(&self) -> HardwareReport {
        let cpuinfo = self.read_cpuinfo_sync();
        let kvm = detect_kvm_support_from_cpuinfo(&cpuinfo);
        HardwareReport {
            cpu: "unknown".to_string(),
            memory_gb: 0,
            disks: Vec::new(),
            nics: Vec::new(),
            kvm_support: kvm,
            warnings: Vec::new(),
        }
    }
}

impl Installer for RustInstaller {
    async fn detect_hardware(&self) -> Result<HardwareReport, IsoError> {
        info!("开始硬件兼容性检测（HCL）");
        // TODO(裸机): 真实探测——
        //   - CPU: 解析 /proc/cpuinfo 的 model name
        //   - 内存: /proc/meminfo 的 MemTotal
        //   - 磁盘: lsblk -b -d -o NAME,SIZE,MODEL,ROTA
        //   - 网卡: /sys/class/net/* 枚举
        //   - KVM: /proc/cpuinfo flags 含 vmx/svm
        let mut report = self.placeholder_report();
        let mut warns = hcl_warnings(&report, &self.thresholds);
        // 占位报告几乎必然触发告警（无盘/无网卡/无 KVM）——这正是骨架阶段的预期
        for w in &warns {
            warn!(warning = %w, "HCL 告警");
        }
        report.warnings.append(&mut warns);
        Ok(report)
    }

    async fn install(
        &self,
        iso_path: &Path,
        target: InstallTarget,
    ) -> Result<InstallReport, IsoError> {
        // 1. 校验 target（必填字段 + RAID 盘数兼容）
        target.validate()?;
        target.validate_raid_disk_count()?;
        if !iso_path.exists() {
            // 注：在沙箱里 ISO 路径可能不存在；此处仅做基础校验。
            warn!(?iso_path, "ISO 路径不存在（骨架阶段可能无真 ISO）");
        }

        info!(?iso_path, disks = ?target.disks, "开始裸机安装");
        let started = Instant::now();

        // 2. 按 InstallStep 状态机推进——构造每步命令（纯函数），真实 spawn 留 runner 注入
        let mut step = InstallStep::Partition;
        let mut pool_created: Option<String> = None;
        let mut installed_components: Vec<String> = Vec::new();
        let mut post_install_actions: Vec<String> = Vec::new();
        // 收集全部构造命令，便于审计 / 沙箱回放 / runner 注入执行
        let mut commands: Vec<(String, Vec<String>)> = Vec::new();
        let raid = target.zfs_raid_level.as_deref();
        let pool_name = "tank";
        let target_root = "/target";
        let target_bin = "/target/usr/local/bin";

        while !step.is_terminal() {
            info!(step = %step.label(), "执行安装步骤（构造命令）");
            // 每步调对应的命令构造纯函数（不真执行——真实 spawn 留 runner 注入）
            match step {
                InstallStep::Partition => {
                    // 多盘：对每块盘构造分区命令
                    for disk in &target.disks {
                        commands.extend(partition_cmd(disk, raid));
                    }
                }
                InstallStep::CreatePool => {
                    let pool = create_pool_cmd(&target.disks, raid, pool_name);
                    commands.push(pool);
                    pool_created = Some(pool_name.to_string());
                }
                InstallStep::ExtractRootfs => {
                    commands.push(extract_rootfs_cmd("rootfs.squashfs", target_root));
                }
                InstallStep::InstallComponents => {
                    // 默认组件清单（呼应 IsoSpec::components；此处用固定清单做骨架）
                    let comps = ["osd", "os-storage", "os-meta"];
                    let cmds = install_components_cmd(&comps, target_bin);
                    commands.extend(cmds);
                    installed_components.extend(comps.iter().map(|s| s.to_string()));
                }
                InstallStep::ConfigureSystem => {
                    let hostname = "os";
                    commands.extend(configure_system_cmd(
                        hostname,
                        &target.locale,
                        &target.admin_user,
                    ));
                }
                InstallStep::SetupFirstBoot => {
                    // 引导安装归入首启编排（bootloader 写入）
                    commands.extend(install_bootloader_cmd(
                        target.disks.first().expect("已校验非空"),
                        target_root,
                    ));
                }
                InstallStep::Done => unreachable!("Done 非终端时不应进入循环"),
            }
            step = step.next();
        }
        // 3. 首启动作清单（§3.19）：强制重设 root 密码 + 初始化管理员
        post_install_actions.push("首次登录强制重设 root 密码".to_string());
        post_install_actions.push(format!("初始化管理员用户: {}", target.admin_user));

        let duration_secs = started.elapsed().as_secs();
        info!(duration_secs, step = %step.label(), "安装完成（骨架：未真实写盘）");
        Ok(InstallReport {
            installed_components,
            pool_created,
            duration_secs,
            post_install_actions,
            commands,
        })
    }
}

impl RustInstaller {
    /// 由外部传入的 `HardwareReport` 推导告警（纯函数包装，便于测试）。
    #[must_use]
    pub fn warnings_for(report: &HardwareReport, thresholds: &HclThresholds) -> Vec<String> {
        hcl_warnings(report, thresholds)
    }

    /// 暴露 `DiskInfo` 的占位构造（测试辅助）。
    #[must_use]
    pub fn placeholder_disk(device: &str, size_gb: u64) -> DiskInfo {
        DiskInfo {
            device: device.to_string(),
            size_gb,
            model: "unknown".to_string(),
            rotational: false,
        }
    }
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::installer::InstallTarget;
    use serde_json::json;
    use std::path::Path;

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

    #[test]
    fn placeholder_disk_shape() {
        let d = RustInstaller::placeholder_disk("/dev/sda", 500);
        assert_eq!(d.device, "/dev/sda");
        assert_eq!(d.size_gb, 500);
        assert!(!d.rotational);
    }

    #[test]
    fn warnings_for_wrapper() {
        let report = HardwareReport {
            cpu: "x".to_string(),
            memory_gb: 2,
            disks: vec![],
            nics: vec![],
            kvm_support: false,
            warnings: vec![],
        };
        let w = RustInstaller::warnings_for(&report, &HclThresholds::default());
        assert!(!w.is_empty());
    }

    #[test]
    fn custom_thresholds_constructor() {
        let inst = RustInstaller::with_thresholds(HclThresholds {
            min_memory_gb: 32,
            recommended_memory_gb: 64,
            min_disk_gb: 500,
        });
        // detect_hardware 用占位报告（mem=0），必触发 min 告警
        // 此处仅验证构造可用
        let _ = inst.thresholds.min_memory_gb;
    }

    #[tokio::test]
    async fn detect_hardware_returns_report_with_warnings() {
        let inst = RustInstaller::new();
        let report = inst.detect_hardware().await.unwrap();
        // 占位报告：几乎必带告警（mem=0, no disk, no nic, no kvm）
        assert!(!report.warnings.is_empty());
        assert!(!report.kvm_support); // 占位 cpuinfo 为空 → false
    }

    #[tokio::test]
    async fn install_invalid_target_errors() {
        let inst = RustInstaller::new();
        let mut t = valid_target();
        t.disks.clear();
        let err = inst
            .install(Path::new("/tmp/none.iso"), t)
            .await
            .unwrap_err();
        assert!(matches!(err, IsoError::InstallFailed(_)));
    }

    #[tokio::test]
    async fn install_raid_disk_count_errors() {
        let inst = RustInstaller::new();
        let mut t = valid_target();
        t.zfs_raid_level = Some("mirror".to_string());
        t.disks = vec!["/dev/sda".to_string()]; // 仅 1 盘
        let err = inst
            .install(Path::new("/tmp/none.iso"), t)
            .await
            .unwrap_err();
        assert!(matches!(err, IsoError::InstallFailed(_)));
    }

    #[tokio::test]
    async fn install_ok_returns_report() {
        let inst = RustInstaller::new();
        let report = inst
            .install(Path::new("/tmp/none.iso"), valid_target())
            .await
            .unwrap();
        assert_eq!(report.pool_created.as_deref(), Some("tank"));
        assert!(report
            .post_install_actions
            .iter()
            .any(|a| a.contains("强制重设 root 密码")));
        assert!(report
            .post_install_actions
            .iter()
            .any(|a| a.contains("初始化管理员用户: admin")));
    }

    #[tokio::test]
    async fn install_admin_user_injected() {
        let inst = RustInstaller::new();
        let mut t = valid_target();
        t.admin_user = "ops".to_string();
        let report = inst.install(Path::new("/tmp/none.iso"), t).await.unwrap();
        assert!(report
            .post_install_actions
            .iter()
            .any(|a| a.contains("初始化管理员用户: ops")));
    }

    #[tokio::test]
    async fn install_clone_raid_levels_ok() {
        // 各 RAID 级别 + 满足盘数应通过
        let inst = RustInstaller::new();
        for (lvl, n) in [("mirror", 2), ("raidz1", 3), ("raidz2", 4), ("raidz3", 5)] {
            let t = InstallTarget {
                disks: (0..n)
                    .map(|i| format!("/dev/sd{}", (b'a' + i as u8) as char))
                    .collect(),
                zfs_raid_level: Some(lvl.to_string()),
                root_password_hash: "$6$x".to_string(),
                admin_user: "admin".to_string(),
                network: json!({}),
                locale: "zh_CN.UTF-8".to_string(),
            };
            let report = inst.install(Path::new("/tmp/none.iso"), t).await.unwrap();
            assert_eq!(report.pool_created.as_deref(), Some("tank"));
        }
    }

    // —— 命令构造接通验证 ——

    #[tokio::test]
    async fn install_collects_commands_single_disk() {
        let inst = RustInstaller::new();
        let report = inst
            .install(Path::new("/tmp/none.iso"), valid_target())
            .await
            .unwrap();
        // 单盘应构造若干命令（分区2 + 建池1 + 解压1 + 组件3 + 配置3 + 引导2 = 12）
        assert!(!report.commands.is_empty(), "应收集到构造的命令");
        // 含 sgdisk（分区）
        assert!(report.commands.iter().any(|(p, _)| p == "sgdisk"));
        // 含 zpool（建池）
        assert!(report.commands.iter().any(|(p, _)| p == "zpool"));
        // 含 unsquashfs（解 rootfs）
        assert!(report.commands.iter().any(|(p, _)| p == "unsquashfs"));
        // 含 cp（装组件）
        assert!(report.commands.iter().any(|(p, _)| p == "cp"));
        // 含 grub-install（装引导）
        assert!(report.commands.iter().any(|(p, _)| p == "grub-install"));
    }

    #[tokio::test]
    async fn install_commands_count_single_disk() {
        let inst = RustInstaller::new();
        let report = inst
            .install(Path::new("/tmp/none.iso"), valid_target())
            .await
            .unwrap();
        // 单盘：分区2(单盘) + 建池1 + 解rootfs1 + 组件3 + 配置3 + 引导2 = 12
        assert_eq!(report.commands.len(), 12);
    }

    #[tokio::test]
    async fn install_commands_pool_uses_stripe_no_keyword() {
        let inst = RustInstaller::new();
        let report = inst
            .install(Path::new("/tmp/none.iso"), valid_target())
            .await
            .unwrap();
        let pool_cmd = report
            .commands
            .iter()
            .find(|(p, _)| p == "zpool")
            .expect("应有 zpool 命令");
        // 单盘 stripe：无 mirror/raidz 关键字
        assert!(!pool_cmd.1.contains(&"mirror".to_string()));
        assert!(!pool_cmd.1.iter().any(|a| a.starts_with("raidz")));
        assert!(pool_cmd.1.contains(&"/dev/sda2".to_string()));
    }

    #[tokio::test]
    async fn install_commands_mirror_has_keyword_and_multi_partition() {
        let inst = RustInstaller::new();
        let t = InstallTarget {
            disks: vec!["/dev/sda".to_string(), "/dev/sdb".to_string()],
            zfs_raid_level: Some("mirror".to_string()),
            root_password_hash: "$6$x".to_string(),
            admin_user: "admin".to_string(),
            network: json!({}),
            locale: "zh_CN.UTF-8".to_string(),
        };
        let report = inst.install(Path::new("/tmp/none.iso"), t).await.unwrap();
        // 镜像：每盘 2 分区命令 = 4 条 sgdisk
        let sgdisk_count = report
            .commands
            .iter()
            .filter(|(p, _)| p == "sgdisk")
            .count();
        assert_eq!(sgdisk_count, 4);
        // 建池命令含 mirror 关键字 + 两块盘分区 2
        let pool_cmd = report
            .commands
            .iter()
            .find(|(p, _)| p == "zpool")
            .expect("应有 zpool");
        assert!(pool_cmd.1.contains(&"mirror".to_string()));
        assert!(pool_cmd.1.contains(&"/dev/sda2".to_string()));
        assert!(pool_cmd.1.contains(&"/dev/sdb2".to_string()));
    }

    #[tokio::test]
    async fn install_commands_locale_injected() {
        let inst = RustInstaller::new();
        let mut t = valid_target();
        t.locale = "en_US.UTF-8".to_string();
        let report = inst.install(Path::new("/tmp/none.iso"), t).await.unwrap();
        // locale-gen 命令应含目标 locale
        let locale_cmd = report
            .commands
            .iter()
            .find(|(p, _)| p == "locale-gen")
            .expect("应有 locale-gen");
        assert!(locale_cmd.1.contains(&"en_US.UTF-8".to_string()));
    }

    #[tokio::test]
    async fn install_commands_admin_in_useradd() {
        let inst = RustInstaller::new();
        let mut t = valid_target();
        t.admin_user = "ops".to_string();
        let report = inst.install(Path::new("/tmp/none.iso"), t).await.unwrap();
        let useradd = report
            .commands
            .iter()
            .find(|(p, _)| p == "useradd")
            .expect("应有 useradd");
        assert!(useradd.1.contains(&"ops".to_string()));
    }

    #[tokio::test]
    async fn install_commands_bootloader_on_first_disk() {
        let inst = RustInstaller::new();
        let t = InstallTarget {
            disks: vec!["/dev/sda".to_string(), "/dev/sdb".to_string()],
            zfs_raid_level: Some("mirror".to_string()),
            root_password_hash: "$6$x".to_string(),
            admin_user: "admin".to_string(),
            network: json!({}),
            locale: "zh_CN.UTF-8".to_string(),
        };
        let report = inst.install(Path::new("/tmp/none.iso"), t).await.unwrap();
        // 引导装第一盘
        let grub_cmds: Vec<_> = report
            .commands
            .iter()
            .filter(|(p, _)| p == "grub-install")
            .collect();
        assert_eq!(grub_cmds.len(), 2, "UEFI + BIOS");
        for (_, args) in &grub_cmds {
            assert!(args.contains(&"/dev/sda".to_string()), "应装到第一盘");
        }
    }
}
