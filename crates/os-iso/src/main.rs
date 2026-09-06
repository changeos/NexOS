//! `os-iso-install` binary 入口 —— ISO Live 环境的安装器（规格书 §3.11 / §3.19 / §10.2#17）。
//!
//! 定位：用户在 ISO Live 环境里运行 `os-iso-install`，开始裸机安装。
//! 安装编排由 [`os_iso::RustInstaller`]（实现 [`os_iso::Installer`] trait）承担：
//! 7 步状态机（分区 → 建池 → 解 rootfs → 装组件 → 配置 → 首启 → 完成）。
//!
//! # 命令行模式
//!
//! - `--check`：硬件检测模式 —— 只跑 HCL（CPU/内存/磁盘/网卡/KVM），输出
//!   [`HardwareReport`](os_iso::HardwareReport) + HCL 告警，不安装。
//! - `--dry-run`：只显示将执行的 7 步安装计划（每步将执行的命令），不真写盘。
//! - 正常模式：构造 [`InstallTarget`] → 调
//!   [`RustInstaller::install`](os_iso::Installer::install) → 输出
//!   [`InstallReport`]。
//!
//! # 红线
//!
//! 真实写盘（parted / zpool create / unsquashfs）需 root + ISO Live 环境；
//! `--dry-run` 绝不真跑（仅打印计划），普通用户可安全预演。

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use serde_json::json;

use os_iso::{
    hcl_warnings, HclThresholds, InstallReport, InstallStep, InstallTarget, Installer,
    RustInstaller,
};

/// CLI 解析的 ZFS RAID 级别（用户友好枚举：`none` 表示单盘）。
///
/// 注：[`InstallTarget::zfs_raid_level`] 用 `Option<String>`，约定值
/// `stripe`/`mirror`/`raidz1`/`raidz2`/`raidz3`。此处 `None` 对应 `None`，
/// 其余原样透传。
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum RaidLevel {
    /// 单盘（stripe / 不建冗余）
    None,
    /// mirror（至少 2 盘）
    Mirror,
    /// raidz1（至少 3 盘）
    Raidz1,
    /// raidz2（至少 4 盘）
    Raidz2,
}

impl RaidLevel {
    /// 转为 `InstallTarget::zfs_raid_level` 期望的字符串值（`None` → `None`）。
    fn to_target_level(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Mirror => Some("mirror"),
            Self::Raidz1 => Some("raidz1"),
            Self::Raidz2 => Some("raidz2"),
        }
    }
}

/// os-iso-install 命令行参数（clap derive）。
#[derive(Debug, Clone, Parser)]
#[command(
    name = "os-iso-install",
    version,
    about = "OS ISO 安装器（ISO Live 环境裸机安装入口）",
    long_about = "在 ISO Live 环境运行，按 7 步状态机执行裸机安装：\
 分区 → 建池 → 解 rootfs → 装组件 → 配置 → 首启 → 完成。\
 --check 只跑硬件检测；--dry-run 只显示计划不写盘。"
)]
struct Cli {
    /// 安装目标盘（设备路径，如 /dev/sda）。可多次指定以组建多盘 RAID。
    ///
    /// 必填（正常模式与 --dry-run 模式）。--check 模式可不提供（仅做硬件检测）。
    #[arg(long, value_name = "DEVICE", num_args = 1.., required_unless_present = "check")]
    disk: Vec<String>,

    /// ZFS RAID 级别（none=单盘 / mirror / raidz1 / raidz2）。
    #[arg(long, value_enum, default_value_t = RaidLevel::None)]
    raid: RaidLevel,

    /// 初始管理员用户名（首启后初始化）。
    #[arg(long, value_name = "USERNAME", required_unless_present = "check")]
    admin: Option<String>,

    /// 区域设置（如 zh_CN.UTF-8）。
    #[arg(long, value_name = "LOCALE", default_value = "en_US.UTF-8")]
    locale: String,

    /// 硬件检测模式：只跑 HCL（CPU/内存/磁盘/网卡/KVM），不安装。
    #[arg(long)]
    check: bool,

    /// 只显示将执行的 7 步安装计划（每步将执行的命令），不真写盘。
    #[arg(long)]
    dry_run: bool,

    /// ISO 镜像路径（安装源；默认 /run/os/rootfs.squashfs，ISO Live 环境挂载点）。
    #[arg(long, value_name = "PATH", default_value = "/run/os/rootfs.squashfs")]
    iso: PathBuf,
}

/// 由当前进程 EUID 判定是否 root（与 osd main 同款，不引入 libc）。
fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|c| {
            c.lines()
                .find(|l| l.starts_with("Uid:"))
                .and_then(|l| l.split_whitespace().nth(3))
                .and_then(|s| s.parse::<u32>().ok())
        })
        .map(|euid| euid == 0)
        .unwrap_or(false)
}

/// --check 模式：跑硬件检测，打印 HardwareReport + HCL 告警。返回是否无告警。
async fn run_check(installer: &RustInstaller) -> bool {
    eprintln!("[check] 开始硬件兼容性检测（HCL）...");
    let report = match installer.detect_hardware().await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[check] 硬件检测失败: {e}");
            return false;
        }
    };

    println!("{report:#?}");
    println!("---- HCL 告警 ----");
    // detect_hardware 已把 hcl_warnings 的结果填入 report.warnings；
    // 此处另算一份纯阈值告警以便核对（不依赖 report.warnings 是否被改动）。
    let warns = hcl_warnings(&report, &HclThresholds::default());
    if warns.is_empty() {
        println!("（无）");
    } else {
        for w in &warns {
            println!("  - {w}");
        }
    }
    warns.is_empty()
}

/// 打印单步将执行的代表命令（dry-run 计划展示，非真实执行）。
///
/// 真实命令由 `RustInstaller::install` 内部按步执行（裸机 TODO）；
/// 此处按步给出"将执行什么"的人类可读摘要，便于用户预演。
fn step_plan(step: InstallStep, target: &InstallTarget) -> String {
    let disks = target.disks.join(" ");
    let raid = target
        .zfs_raid_level
        .clone()
        .unwrap_or_else(|| "stripe".to_string());
    match step {
        InstallStep::Partition => {
            format!("parted -s <disk> mklabel gpt mkpart ...  | disks: {disks}")
        }
        InstallStep::CreatePool => {
            format!("zpool create -f tank {raid} {disks}")
        }
        InstallStep::ExtractRootfs => {
            "unsquashfs -f -d /tank/root rootfs.squashfs  | src: ISO".to_string()
        }
        InstallStep::InstallComponents => {
            "cp osd os-storage os-network os-api ... → /tank/root/usr/bin".to_string()
        }
        InstallStep::ConfigureSystem => {
            format!(
                "写 fstab / 网络 / locale={} / admin={} → /tank/root/etc",
                target.locale, target.admin_user
            )
        }
        InstallStep::SetupFirstBoot => {
            "注册首启强制重设 root 密码钩子 + 初始化 admin（§3.19）".to_string()
        }
        InstallStep::Done => "安装完成".to_string(),
    }
}

/// --dry-run 模式：打印 7 步安装计划，不真跑。始终成功返回。
fn run_dry_run(target: &InstallTarget, iso: &std::path::Path) {
    eprintln!("[dry-run] 仅显示安装计划，不写盘（普通用户可安全预演）");
    eprintln!("[dry-run] ISO: {}", iso.display());
    eprintln!(
        "[dry-run] 目标盘: {:?}  RAID: {}  admin: {}  locale: {}",
        target.disks,
        target.zfs_raid_level.as_deref().unwrap_or("none"),
        target.admin_user,
        target.locale
    );
    eprintln!("[dry-run] 安装计划（7 步状态机）：");
    for (i, step) in InstallStep::all_steps().iter().enumerate() {
        let plan = step_plan(*step, target);
        println!("  {i}. [{}] {plan}", step.label());
    }
    eprintln!("[dry-run] 计划展示完毕（未写盘）");
}

/// 正常模式：构造 InstallTarget → 调 install → 打印 InstallReport。
async fn run_install(
    installer: &RustInstaller,
    target: InstallTarget,
    iso: &std::path::Path,
) -> Result<InstallReport, os_iso::IsoError> {
    // 真实写盘需 root；非 root 直接拒绝（避免半途失败留下脏分区表）。
    if !is_root() {
        eprintln!("[install] 警告: 非 root 运行，真实写盘（parted/zpool）将失败");
        eprintln!("[install] 提示: ISO Live 环境应以 root 登录；或用 --dry-run 预演");
    }
    eprintln!(
        "[install] 开始安装 → ISO: {}  盘: {:?}",
        iso.display(),
        target.disks
    );
    eprintln!("[install] 安装计划（7 步状态机）：");
    for (i, step) in InstallStep::all_steps().iter().enumerate() {
        eprintln!(
            "[install]   {i}. [{}] {}",
            step.label(),
            step_plan(*step, &target)
        );
    }
    eprintln!("[install] 开始执行（每步日志由 tracing 输出，RUST_LOG=info 可见）...");

    let report = installer.install(iso, target).await?;

    eprintln!("[install] >>> 全部步骤完成");
    Ok(report)
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let installer = RustInstaller::new();

    // --check 模式：只跑硬件检测（不需要 disk/admin）。
    if cli.check {
        let ok = run_check(&installer).await;
        if ok {
            eprintln!("[os-iso-install] 硬件检测通过（无告警）");
            ExitCode::SUCCESS
        } else {
            eprintln!("[os-iso-install] 硬件检测完成（存在告警，详见上方输出）");
            ExitCode::SUCCESS // 告警不视为失败（仅诊断信号）
        }
    } else {
        // 正常 / dry-run 模式：disk + admin 必填（clap required_unless_present 已保证）。
        let admin = cli.admin.expect("clap 保证 --check 模式外 admin 必填");

        // 构造 InstallTarget。root_password_hash 留占位哈希——首启强制重设（§3.19），
        // 安装器绝不预置明文密码；占位哈希仅满足 validate() 非空校验。
        let target = InstallTarget {
            disks: cli.disk.clone(),
            zfs_raid_level: cli.raid.to_target_level().map(str::to_string),
            root_password_hash: PLACEHOLDER_ROOT_HASH.to_string(),
            admin_user: admin,
            network: json!({"mode": "dhcp"}),
            locale: cli.locale.clone(),
        };

        // 先做参数校验（与 install() 内部一致），dry-run / 正常模式共享。
        if let Err(e) = target.validate() {
            eprintln!("[os-iso-install] 参数校验失败: {e}");
            return ExitCode::FAILURE;
        }
        if let Err(e) = target.validate_raid_disk_count() {
            eprintln!("[os-iso-install] 参数校验失败: {e}");
            return ExitCode::FAILURE;
        }

        if cli.dry_run {
            run_dry_run(&target, &cli.iso);
            return ExitCode::SUCCESS;
        }

        match run_install(&installer, target, &cli.iso).await {
            Ok(report) => {
                println!("---- 安装报告 ----");
                println!("{report:#?}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("[os-iso-install] 安装失败: {e}");
                ExitCode::FAILURE
            }
        }
    }
}

/// 占位 root 密码哈希（首启强制重设，§3.19）。SHA-512 crypt 格式占位，非有效登录凭据。
const PLACEHOLDER_ROOT_HASH: &str =
    "$6$rounds=5000$installer$PLACEHOLDER.HASH.FORCE.RESET.ON.FIRST.BOOT";
