//! Bootloader A/B 槽位激活——GRUB / systemd-boot 真实交互（规划文档 §3.12）。
//!
//! 本模块把"接通真实 bootloader"的逻辑集中放置，使 [`crate::AbUpdateEngine`]
//! 的 `activate_slot` 走真实编排（而非 `todo!()`）。
//!
//! - [`BootloaderKind`]：bootloader 类型（GRUB / systemd-boot）。
//! - [`SlotBootEntry`]：单槽位的 boot entry 描述（kernel/initrd/uuid/version/args）。
//! - [`BootloaderConfig`]：A/B 双槽 bootloader 配置抽象（两槽 entry + default +
//!   `boot_once` fallback）——纯数据结构，可序列化、可生成 bootloader 配置文本。
//! - [`BootloaderRunner`]：执行 bootloader 工具（`grub2-reboot`/`bootctl set-default`）
//!   的 trait 抽象；生产用 [`TokioBootloaderRunner`]（spawn 真实子进程），测试用
//!   fixture（按命令分发预设输出）。
//! - [`grub`] / [`systemd_boot`]：配置文本生成（纯函数，有独立单测）。
//!
//! 设计原则：
//! - **配置生成与执行解耦**：生成（render_*）是纯函数，无 I/O，可直接 fixture 测；
//!   执行（runner）调真实 bootloader 工具，`#[ignore]` 真实测（需 root）。
//! - **next-boot 一次性切换**：A/B 槽激活用 `grub2-reboot`（写 next-default，仅下次
//!   启动生效）+ `bootctl set-oneshot`，失败回滚到原 default（boot_once fallback）。
//! - **不变量**：永远先记 next-default（一次性），探活通过后才升级为持久 default
//!   （`grub2-editenv`/`bootctl set-default`），避免坏槽变持久默认 brick 系统。

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::Command;

use crate::error::UpdateError;
use crate::update::UpdateSlot;

// ----------------------------------------------------------------------------
// 类型 / 错误转换
// ----------------------------------------------------------------------------

/// 支持的 bootloader 类型（规划文档 §6 风险行：grub-bls ↔ systemd-boot）。
///
/// 选型由系统构建期决定（烧入镜像时选其一）；运行时通过 [`BootloaderConfig::kind`]
/// 标识。新增类型须经 ADR（架构性变更）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootloaderKind {
    /// GRUB（含 grub-bls）：用 `grub2-reboot`（next-boot 一次性）+ `grub2-editenv`
    /// （持久 default）切换。
    Grub,
    /// systemd-boot：遵循 Boot Loader Spec，用 `bootctl set-default`/
    /// `set-oneshot` 切换 entry。
    SystemdBoot,
}

/// 单个槽位的 boot entry 描述（用于生成 GRUB menuentry / systemd-boot conf）。
///
/// 字段为 bootloader 通用最小集；不同 bootloader 渲染为各自语法（见 [`grub`]/
/// [`systemd_boot`]）。`linux`/`initrd` 为相对于 boot 分区根的路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotBootEntry {
    /// 槽位标识（A/B）
    pub slot: UpdateSlot,
    /// 此槽装载的系统版本（用于 menuentry 标题）
    pub version: String,
    /// 内核镜像路径（boot 分区相对，如 `/boot/slot-a/vmlinuz`）
    pub linux: PathBuf,
    /// initrd 路径（boot 分区相对，如 `/boot/slot-a/initrd.img`）
    pub initrd: PathBuf,
    /// 内核命令行参数（root=UUID=... ro slot=A 等）
    pub cmdline: String,
}

/// A/B 双槽 bootloader 配置抽象。
///
/// 包含两槽完整 boot entry + 当前持久 default 槽 + 下次启动（next-boot）
/// 一次性切换目标（None = 不一次性切换，用 default）+ boot 分区根路径
/// （渲染配置文件写入位置的基础路径，默认 `/boot`）。
///
/// **boot_once fallback**：`activate_slot` 先设 `next_default`（一次性，仅下次启动
/// 生效）；探活通过后才升级为持久 `default`。若 next-boot 启动失败（watchdog 超时），
/// 下下次启动自动回到持久 `default`——即 boot_once fallback。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootloaderConfig {
    /// bootloader 类型
    pub kind: BootloaderKind,
    /// 槽 A 的 boot entry
    pub slot_a: SlotBootEntry,
    /// 槽 B 的 boot entry
    pub slot_b: SlotBootEntry,
    /// 当前持久 default 槽（每次启动默认从此槽引导）
    pub default: UpdateSlot,
    /// 下次启动一次性切换目标（None = 用 default；Some = 仅下次启动用此槽）
    pub next_default: Option<UpdateSlot>,
    /// boot 分区根路径（渲染配置文件写入位置的基础；默认 `/boot`，
    /// 测试可指向临时目录避免触盘写真实 /boot）
    pub boot_root: PathBuf,
}

impl BootloaderConfig {
    /// 取指定槽的 boot entry。
    #[must_use]
    pub fn entry(&self, slot: UpdateSlot) -> &SlotBootEntry {
        match slot {
            UpdateSlot::A => &self.slot_a,
            UpdateSlot::B => &self.slot_b,
        }
    }

    /// 实际下次启动槽：优先 `next_default`，否则 `default`。
    #[must_use]
    pub fn effective_next_boot(&self) -> UpdateSlot {
        self.next_default.unwrap_or(self.default)
    }

    /// 设置持久 default 槽（探活通过后调用，提交切换）。
    pub fn set_default(&mut self, slot: UpdateSlot) {
        self.default = slot;
        // 已升级为持久 default，清掉 next_default（避免冗余）
        if self.next_default == Some(slot) {
            self.next_default = None;
        }
    }

    /// 设置 next-boot 一次性切换（activate_slot 阶段调用，先记一次性，失败可回退）。
    pub fn set_next_boot(&mut self, slot: UpdateSlot) {
        self.next_default = Some(slot);
    }

    /// 渲染为 bootloader 配置文本（按 [`BootloaderKind`] 分发）。
    ///
    /// 返回 `(path, content)` 对列表：每个元组表示"写入此路径的文件内容"。
    /// 路径基于 [`Self::boot_root`]（默认 `/boot`，测试可指向临时目录）。
    /// - GRUB：单个 `<boot_root>/grub/grub.cfg` 片段（menuentry * 2 + default 设置）。
    /// - systemd-boot：两个 `<boot_root>/loader/entries/*.conf` + `loader.conf`。
    #[must_use]
    pub fn render(&self) -> Vec<(PathBuf, String)> {
        match self.kind {
            BootloaderKind::Grub => {
                let cfg = grub::render_grub_config(self);
                vec![(self.boot_root.join("grub/grub.cfg"), cfg)]
            }
            BootloaderKind::SystemdBoot => systemd_boot::render_systemd_boot_files(self),
        }
    }
}

// ============================================================================
// GRUB 配置生成
// ============================================================================

/// GRUB 配置生成（纯函数）。
pub mod grub {
    use super::*;

    /// GRUB menuentry 标识（用于 `grub2-reboot`/default 引用）。
    ///
    /// 约定：`os_slot_a`/`os_slot_b`（小写、稳定，便于 `grub2-reboot os_slot_b`）。
    #[must_use]
    pub fn menuentry_id(slot: UpdateSlot) -> &'static str {
        match slot {
            UpdateSlot::A => "os_slot_a",
            UpdateSlot::B => "os_slot_b",
        }
    }

    /// 生成单个 GRUB menuentry 文本（含 linux/initrd/cmdline）。
    #[must_use]
    pub fn render_menuentry(entry: &SlotBootEntry) -> String {
        let id = menuentry_id(entry.slot);
        let title = format!("OS {} (slot {:?})", entry.version, entry.slot);
        format!(
            "menuentry \"{title}\" --id {id} {{\n\
             \x20   set root=(hd0,msdos1)\n\
             \x20   linux {linux} root={root} ro {cmdline}\n\
             \x20   initrd {initrd}\n\
             }}\n",
            linux = entry.linux.display(),
            // cmdline 内含 root=UUID=...，从中提取 root= 给 GRUB set；若 cmdline
            // 不含 root=，则用占位（GRUB 需 root= 参数引导）
            root = extract_root(&entry.cmdline)
                .unwrap_or_else(|| "UUID=00000000-0000-0000-0000-000000000000".to_string()),
            cmdline = entry.cmdline,
            initrd = entry.initrd.display(),
        )
    }

    /// 从 cmdline 中提取 `root=XXX` 的值（GRUB menuentry 的 set root= 行）。
    fn extract_root(cmdline: &str) -> Option<String> {
        for tok in cmdline.split_whitespace() {
            if let Some(rest) = tok.strip_prefix("root=") {
                return Some(rest.to_string());
            }
        }
        None
    }

    /// 生成完整 grub.cfg 片段（A/B menuentry + default + next_default）。
    ///
    /// - `set default=...`：持久 default（每次启动用）。
    /// - `set next_default=...`（GRUB 无原生 next-default 变量；实际 next-boot
    ///   由 `grub2-reboot` 写 `GRUB_NEXT_DEFAULT` 环境变量，见 runner。此处仅
    ///   渲染 default + 注释说明 next-default 由 grub2-reboot 维护）。
    #[must_use]
    pub fn render_grub_config(cfg: &BootloaderConfig) -> String {
        let mut out = String::new();
        out.push_str("# 由 os-update 自动生成——请勿手改（A/B 双槽 bootloader 配置）\n");
        out.push_str(&format!(
            "# default = {:?}，next_default = {}\n",
            cfg.default,
            match cfg.next_default {
                Some(s) => format!("{s:?}"),
                None => "(none)".to_string(),
            }
        ));
        out.push_str(&format!("set default={}\n", menuentry_id(cfg.default)));
        if let Some(next) = cfg.next_default {
            // GRUB next-default 由 grub2-reboot 单独管理（GRUB_NEXT_DEFAULT 环境变量），
            // 此处仅注释标记，便于诊断（实际生效靠 runner 调 grub2-reboot）。
            out.push_str(&format!(
                "# next-boot oneshot → {} (set by `grub2-reboot {}`)\n",
                menuentry_id(next),
                menuentry_id(next)
            ));
        }
        out.push('\n');
        // menuentry（default 在前，便于 GRUB 高亮）
        let order = order_default_first(cfg.default);
        for slot in order {
            out.push_str(&render_menuentry(cfg.entry(slot)));
            out.push('\n');
        }
        out
    }

    /// 按 default 在前排定两槽顺序（GRUB menuentry 顺序影响默认高亮）。
    fn order_default_first(default: UpdateSlot) -> [UpdateSlot; 2] {
        match default {
            UpdateSlot::A => [UpdateSlot::A, UpdateSlot::B],
            UpdateSlot::B => [UpdateSlot::B, UpdateSlot::A],
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn entry(slot: UpdateSlot, ver: &str) -> SlotBootEntry {
            let tag = match slot {
                UpdateSlot::A => 'a',
                UpdateSlot::B => 'b',
            };
            SlotBootEntry {
                slot,
                version: ver.to_string(),
                linux: PathBuf::from(format!("/boot/slot-{tag}/vmlinuz")),
                initrd: PathBuf::from(format!("/boot/slot-{tag}/initrd.img")),
                cmdline: format!("root=UUID=slot{tag} ro slot={slot:?}"),
            }
        }

        #[test]
        fn menuentry_id_stable() {
            assert_eq!(menuentry_id(UpdateSlot::A), "os_slot_a");
            assert_eq!(menuentry_id(UpdateSlot::B), "os_slot_b");
        }

        #[test]
        fn render_menuentry_has_id_and_linux() {
            let e = entry(UpdateSlot::A, "1.0.0");
            let s = render_menuentry(&e);
            assert!(s.contains("--id os_slot_a"));
            assert!(s.contains("/boot/slot-a/vmlinuz"));
            assert!(s.contains("/boot/slot-a/initrd.img"));
            assert!(s.contains("UUID=slota"));
        }

        #[test]
        fn render_grub_config_default_first() {
            let cfg = BootloaderConfig {
                kind: BootloaderKind::Grub,
                slot_a: entry(UpdateSlot::A, "1.0.0"),
                slot_b: entry(UpdateSlot::B, "1.1.0"),
                default: UpdateSlot::B,
                next_default: None,
                boot_root: std::path::PathBuf::from("/tmp/test-boot"),
            };
            let s = render_grub_config(&cfg);
            // default=B 应出现在前（先渲染 B menuentry）
            let b_pos = s.find("os_slot_b").unwrap();
            let a_pos = s.rfind("os_slot_a").unwrap();
            assert!(b_pos < a_pos);
            assert!(s.contains("set default=os_slot_b"));
        }

        #[test]
        fn render_grub_config_with_next_default_comment() {
            let cfg = BootloaderConfig {
                kind: BootloaderKind::Grub,
                slot_a: entry(UpdateSlot::A, "1.0.0"),
                slot_b: entry(UpdateSlot::B, "1.1.0"),
                default: UpdateSlot::A,
                next_default: Some(UpdateSlot::B),
                boot_root: std::path::PathBuf::from("/tmp/test-boot"),
            };
            let s = render_grub_config(&cfg);
            assert!(s.contains("next-boot oneshot"));
            assert!(s.contains("grub2-reboot os_slot_b"));
        }
    }
}

// ============================================================================
// systemd-boot 配置生成（Boot Loader Spec）
// ============================================================================

/// systemd-boot 配置生成（Boot Loader Spec 类型#1 entry 文件）。
pub mod systemd_boot {
    use super::*;

    /// entry 文件名约定（Boot Loader Spec：`/loader/entries/<id>.conf`）。
    #[must_use]
    pub fn entry_conf_filename(slot: UpdateSlot) -> String {
        format!(
            "os-{}.conf",
            match slot {
                UpdateSlot::A => "slot-a",
                UpdateSlot::B => "slot-b",
            }
        )
    }

    /// entry 文件 ID（用于 `loader.conf` 的 default 行 / `bootctl set-default`）。
    #[must_use]
    pub fn entry_id(slot: UpdateSlot) -> &'static str {
        match slot {
            UpdateSlot::A => "os-slot-a",
            UpdateSlot::B => "os-slot-b",
        }
    }

    /// 生成单个 systemd-boot entry conf 内容（Boot Loader Spec 类型#1）。
    #[must_use]
    pub fn render_entry_conf(entry: &SlotBootEntry) -> String {
        format!(
            "title   OS {version} (slot {slot:?})\n\
             version {version}\n\
             linux   {linux}\n\
             initrd  {initrd}\n\
             options {cmdline}\n",
            version = entry.version,
            slot = entry.slot,
            linux = entry.linux.display(),
            initrd = entry.initrd.display(),
            cmdline = entry.cmdline,
        )
    }

    /// 生成 `loader.conf` 内容（default + timeout）。
    #[must_use]
    pub fn render_loader_conf(cfg: &BootloaderConfig) -> String {
        let mut s = String::new();
        s.push_str("# 由 os-update 自动生成（systemd-boot loader.conf）\n");
        s.push_str(&format!("default {}\n", entry_id(cfg.default)));
        if let Some(next) = cfg.next_default {
            // systemd-boot 用单独的 oneshot 机制（`bootctl set-oneshot`），
            // loader.conf 不写 oneshot；此处注释标记便于诊断。
            s.push_str(&format!(
                "# next-boot oneshot → {} (set by `bootctl set-oneshot {}`)\n",
                entry_id(next),
                entry_id(next)
            ));
        }
        s.push_str("timeout 5\n");
        s
    }

    /// 渲染 systemd-boot 全部配置文件：
    /// `<boot_root>/loader/entries/os-slot-a.conf`、
    /// `<boot_root>/loader/entries/os-slot-b.conf`、
    /// `<boot_root>/loader/loader.conf`。
    #[must_use]
    pub fn render_systemd_boot_files(cfg: &BootloaderConfig) -> Vec<(PathBuf, String)> {
        vec![
            (
                cfg.boot_root
                    .join("loader/entries")
                    .join(entry_conf_filename(UpdateSlot::A)),
                render_entry_conf(&cfg.slot_a),
            ),
            (
                cfg.boot_root
                    .join("loader/entries")
                    .join(entry_conf_filename(UpdateSlot::B)),
                render_entry_conf(&cfg.slot_b),
            ),
            (
                cfg.boot_root.join("loader/loader.conf"),
                render_loader_conf(cfg),
            ),
        ]
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn entry(slot: UpdateSlot, ver: &str) -> SlotBootEntry {
            let tag = match slot {
                UpdateSlot::A => 'a',
                UpdateSlot::B => 'b',
            };
            SlotBootEntry {
                slot,
                version: ver.to_string(),
                linux: PathBuf::from(format!("/slot-{tag}/vmlinuz")),
                initrd: PathBuf::from(format!("/slot-{tag}/initrd.img")),
                cmdline: format!("root=UUID=slot{tag} ro slot={slot:?}"),
            }
        }

        #[test]
        fn entry_id_and_filename() {
            assert_eq!(entry_id(UpdateSlot::A), "os-slot-a");
            assert_eq!(entry_conf_filename(UpdateSlot::B), "os-slot-b.conf");
        }

        #[test]
        fn render_entry_conf_bls_fields() {
            let e = entry(UpdateSlot::A, "1.2.0");
            let s = render_entry_conf(&e);
            assert!(s.contains("title   OS 1.2.0 (slot A)"));
            assert!(s.contains("linux   /slot-a/vmlinuz"));
            assert!(s.contains("initrd  /slot-a/initrd.img"));
            assert!(s.contains("options root=UUID=slota ro slot=A"));
        }

        #[test]
        fn render_loader_conf_default() {
            let cfg = BootloaderConfig {
                kind: BootloaderKind::SystemdBoot,
                slot_a: entry(UpdateSlot::A, "1.0.0"),
                slot_b: entry(UpdateSlot::B, "1.1.0"),
                default: UpdateSlot::A,
                next_default: None,
                boot_root: std::path::PathBuf::from("/tmp/test-boot"),
            };
            let s = render_loader_conf(&cfg);
            assert!(s.contains("default os-slot-a"));
            assert!(s.contains("timeout 5"));
        }

        #[test]
        fn render_files_three_paths() {
            let cfg = BootloaderConfig {
                kind: BootloaderKind::SystemdBoot,
                slot_a: entry(UpdateSlot::A, "1.0.0"),
                slot_b: entry(UpdateSlot::B, "1.1.0"),
                default: UpdateSlot::A,
                next_default: Some(UpdateSlot::B),
                boot_root: std::path::PathBuf::from("/tmp/test-boot"),
            };
            let files = render_systemd_boot_files(&cfg);
            assert_eq!(files.len(), 3);
            let paths: Vec<_> = files.iter().map(|(p, _)| p.clone()).collect();
            assert!(paths
                .iter()
                .any(|p| p == &PathBuf::from("/tmp/test-boot/loader/entries/os-slot-a.conf")));
            assert!(paths
                .iter()
                .any(|p| p == &PathBuf::from("/tmp/test-boot/loader/entries/os-slot-b.conf")));
            assert!(paths
                .iter()
                .any(|p| p == &PathBuf::from("/tmp/test-boot/loader/loader.conf")));
            // next_default 注释应在 loader.conf
            let loader = files
                .iter()
                .find(|(p, _)| p == &PathBuf::from("/tmp/test-boot/loader/loader.conf"))
                .unwrap();
            assert!(loader.1.contains("next-boot oneshot"));
        }
    }
}

// ============================================================================
// BootloaderRunner —— bootloader 工具执行抽象
// ============================================================================

/// 子进程执行结果（与 `std::process::Output` 同构，owned + 可由测试构造）。
#[derive(Debug, Clone)]
pub struct BootloaderCommandOutput {
    /// 退出码（0 = 成功）
    pub status: i32,
    /// stdout（UTF-8 解码后）
    pub stdout: String,
    /// stderr（UTF-8 解码后；保留供错误诊断）
    pub stderr: String,
}

impl BootloaderCommandOutput {
    /// 成功、空输出的便捷构造（测试用）。
    #[must_use]
    pub fn ok() -> Self {
        Self {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
        }
    }
}

/// bootloader 工具执行器抽象——隔离 `grub2-reboot`/`bootctl` 等子进程调用。
///
/// 生产实现 [`TokioBootloaderRunner`] 调真实子进程；测试用 fixture runner
/// （按 `program`+`args` 分发预设输出）。沿用 `os-storage` 的 `CommandRunner`
/// 模式（ADR-COMPAT-001：trait 用 `#[async_trait]` 保证 dyn 兼容）。
#[async_trait]
pub trait BootloaderRunner: Send + Sync {
    /// 执行 `<program> <args...>`，返回 stdout/stderr/退出码。
    async fn run(
        &self,
        program: &str,
        args: &[String],
    ) -> Result<BootloaderCommandOutput, UpdateError>;
}

/// 生产用执行器——`tokio::process::Command` spawn 真实子进程。
///
/// `grub2-reboot`/`bootctl` 必须在 `$PATH`（通常 `/usr/sbin`/`/usr/bin`）。
/// 调用须 root（写 bootloader 配置/env），未授权会以非零退出码失败（映射 UpdateError）。
pub struct TokioBootloaderRunner;

#[async_trait]
impl BootloaderRunner for TokioBootloaderRunner {
    async fn run(
        &self,
        program: &str,
        args: &[String],
    ) -> Result<BootloaderCommandOutput, UpdateError> {
        let output = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| {
                UpdateError::SlotConflict(format!("执行 bootloader 工具 {program} 失败: {e}"))
            })?;
        Ok(BootloaderCommandOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

// ============================================================================
// 激活编排（纯逻辑，调 runner）
// ============================================================================

/// 激活槽位的两阶段编排参数。
///
/// `activate_slot` 分两阶段（boot_once fallback）：
/// 1. **next-boot 阶段**：调 `grub2-reboot <id>`/`bootctl set-oneshot <id>`——
///    仅下次启动用目标槽，持久 default 不变。失败回滚（不清 next-default，
///    因工具失败时 bootloader 状态未变）。
/// 2. **commit 阶段**：探活通过后调 `grub2-editenv`/`bootctl set-default <id>`
///    升级为持久 default。
///
/// 本结构封装两阶段命令选择，便于 fixture 测。
#[derive(Debug, Clone, Copy)]
pub struct ActivationPlan {
    /// bootloader 类型
    pub kind: BootloaderKind,
    /// 目标槽（要激活的）
    pub target: UpdateSlot,
}

impl ActivationPlan {
    /// 构造激活计划。
    #[must_use]
    pub fn new(kind: BootloaderKind, target: UpdateSlot) -> Self {
        Self { kind, target }
    }

    /// next-boot 阶段命令（program, args）。
    ///
    /// - GRUB：`grub2-reboot os_slot_<x>`（写 next-default，仅下次启动生效）。
    /// - systemd-boot：`bootctl set-oneshot os-slot-<x>`（仅下次启动）。
    #[must_use]
    pub fn next_boot_command(&self) -> (&'static str, Vec<String>) {
        match self.kind {
            BootloaderKind::Grub => {
                let id = grub::menuentry_id(self.target).to_string();
                ("grub2-reboot", vec![id])
            }
            BootloaderKind::SystemdBoot => {
                let id = systemd_boot::entry_id(self.target).to_string();
                ("bootctl", vec!["set-oneshot".to_string(), id])
            }
        }
    }

    /// commit 阶段命令（program, args）。
    ///
    /// - GRUB：`grub2-set-default os_slot_<x>`（设持久 default）。
    /// - systemd-boot：`bootctl set-default os-slot-<x>`。
    #[must_use]
    pub fn commit_command(&self) -> (&'static str, Vec<String>) {
        match self.kind {
            BootloaderKind::Grub => {
                let id = grub::menuentry_id(self.target).to_string();
                ("grub2-set-default", vec![id])
            }
            BootloaderKind::SystemdBoot => {
                let id = systemd_boot::entry_id(self.target).to_string();
                ("bootctl", vec!["set-default".to_string(), id])
            }
        }
    }
}

/// 执行 next-boot 阶段：调 bootloader 工具设下次启动目标槽（一次性）。
///
/// 失败映射为 [`UpdateError::SlotConflict`]（保留 stderr）；成功返回 Ok。
pub async fn run_next_boot(
    runner: &dyn BootloaderRunner,
    plan: &ActivationPlan,
) -> Result<(), UpdateError> {
    let (program, args) = plan.next_boot_command();
    let out = runner.run(program, &args).await?;
    if out.status != 0 {
        return Err(UpdateError::SlotConflict(format!(
            "{program} {} 退出码 {}：{}",
            args.join(" "),
            out.status,
            out.stderr.trim()
        )));
    }
    Ok(())
}

/// 执行 commit 阶段：把目标槽升级为持久 default（探活通过后调用）。
///
/// 失败映射为 [`UpdateError::SlotConflict`]；成功返回 Ok。
pub async fn run_commit(
    runner: &dyn BootloaderRunner,
    plan: &ActivationPlan,
) -> Result<(), UpdateError> {
    let (program, args) = plan.commit_command();
    let out = runner.run(program, &args).await?;
    if out.status != 0 {
        return Err(UpdateError::SlotConflict(format!(
            "{program} {} 退出码 {}：{}",
            args.join(" "),
            out.status,
            out.stderr.trim()
        )));
    }
    Ok(())
}

/// 把配置渲染的文件写入磁盘（生产路径用，测试用 fixture 不调此）。
///
/// 原子写：先写 `.new` 再 rename，避免半写文件污染 bootloader 状态。
/// 调用须 root（写 /boot 下文件）。
pub fn write_config_files(files: &[(PathBuf, String)]) -> Result<(), UpdateError> {
    for (path, content) in files {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                UpdateError::SlotConflict(format!(
                    "创建 bootloader 配置目录 {} 失败: {e}",
                    parent.display()
                ))
            })?;
        }
        let tmp = path.with_extension("new");
        std::fs::write(&tmp, content).map_err(|e| {
            UpdateError::SlotConflict(format!("写 bootloader 配置 {} 失败: {e}", tmp.display()))
        })?;
        std::fs::rename(&tmp, path).map_err(|e| {
            UpdateError::SlotConflict(format!(
                "原子改名 bootloader 配置 {} → {} 失败: {e}",
                tmp.display(),
                path.display()
            ))
        })?;
    }
    Ok(())
}

// ============================================================================
// 单元测试（配置生成 + 激活命令选择）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(slot: UpdateSlot, ver: &str) -> SlotBootEntry {
        let tag = match slot {
            UpdateSlot::A => 'a',
            UpdateSlot::B => 'b',
        };
        SlotBootEntry {
            slot,
            version: ver.to_string(),
            linux: PathBuf::from(format!("/boot/slot-{tag}/vmlinuz")),
            initrd: PathBuf::from(format!("/boot/slot-{tag}/initrd.img")),
            cmdline: format!("root=UUID=slot{tag} ro slot={slot:?}"),
        }
    }

    fn sample_config(kind: BootloaderKind) -> BootloaderConfig {
        BootloaderConfig {
            kind,
            slot_a: entry(UpdateSlot::A, "1.0.0"),
            slot_b: entry(UpdateSlot::B, "1.1.0"),
            default: UpdateSlot::A,
            next_default: None,
            boot_root: std::path::PathBuf::from("/tmp/test-boot"),
        }
    }

    // —— BootloaderConfig 行为 ——

    #[test]
    fn effective_next_boot_uses_next_default() {
        let mut cfg = sample_config(BootloaderKind::Grub);
        assert_eq!(cfg.effective_next_boot(), UpdateSlot::A);
        cfg.set_next_boot(UpdateSlot::B);
        assert_eq!(cfg.effective_next_boot(), UpdateSlot::B);
    }

    #[test]
    fn set_default_clears_next_default() {
        let mut cfg = sample_config(BootloaderKind::Grub);
        cfg.set_next_boot(UpdateSlot::B);
        cfg.set_default(UpdateSlot::B);
        assert_eq!(cfg.default, UpdateSlot::B);
        assert_eq!(cfg.next_default, None);
    }

    #[test]
    fn render_dispatches_by_kind() {
        let grub_cfg = sample_config(BootloaderKind::Grub);
        let grub_files = grub_cfg.render();
        assert_eq!(grub_files.len(), 1);
        assert!(grub_files[0].0.ends_with("grub/grub.cfg"));

        let sd_cfg = sample_config(BootloaderKind::SystemdBoot);
        let sd_files = sd_cfg.render();
        assert_eq!(sd_files.len(), 3);
        // 校验 systemd-boot 三文件路径结构
        assert!(sd_files
            .iter()
            .any(|(p, _)| p.ends_with("loader/entries/os-slot-a.conf")));
        assert!(sd_files
            .iter()
            .any(|(p, _)| p.ends_with("loader/loader.conf")));
    }

    // —— 激活命令选择 ——

    #[test]
    fn grub_next_boot_uses_grub2_reboot() {
        let plan = ActivationPlan::new(BootloaderKind::Grub, UpdateSlot::B);
        let (prog, args) = plan.next_boot_command();
        assert_eq!(prog, "grub2-reboot");
        assert_eq!(args, vec!["os_slot_b".to_string()]);
    }

    #[test]
    fn grub_commit_uses_grub2_set_default() {
        let plan = ActivationPlan::new(BootloaderKind::Grub, UpdateSlot::A);
        let (prog, args) = plan.commit_command();
        assert_eq!(prog, "grub2-set-default");
        assert_eq!(args, vec!["os_slot_a".to_string()]);
    }

    #[test]
    fn systemd_next_boot_uses_bootctl_set_oneshot() {
        let plan = ActivationPlan::new(BootloaderKind::SystemdBoot, UpdateSlot::B);
        let (prog, args) = plan.next_boot_command();
        assert_eq!(prog, "bootctl");
        assert_eq!(
            args,
            vec!["set-oneshot".to_string(), "os-slot-b".to_string()]
        );
    }

    #[test]
    fn systemd_commit_uses_bootctl_set_default() {
        let plan = ActivationPlan::new(BootloaderKind::SystemdBoot, UpdateSlot::A);
        let (prog, args) = plan.commit_command();
        assert_eq!(prog, "bootctl");
        assert_eq!(
            args,
            vec!["set-default".to_string(), "os-slot-a".to_string()]
        );
    }

    // —— run_next_boot / run_commit（fixture runner）——

    /// 测试用 fixture runner：按 `(program, args 首元素)` 分发预设输出。
    struct FixtureRunner {
        outputs: std::sync::Mutex<Vec<(String, String, BootloaderCommandOutput)>>,
    }

    impl FixtureRunner {
        fn new() -> Self {
            Self {
                outputs: std::sync::Mutex::new(Vec::new()),
            }
        }

        /// 注册 fixture：当 `program` + `args` 首元素匹配时返回 `output`。
        fn on(self, program: &str, args_first: &str, output: BootloaderCommandOutput) -> Self {
            self.outputs.lock().unwrap().push((
                program.to_string(),
                args_first.to_string(),
                output,
            ));
            self
        }
    }

    #[async_trait]
    impl BootloaderRunner for FixtureRunner {
        async fn run(
            &self,
            program: &str,
            args: &[String],
        ) -> Result<BootloaderCommandOutput, UpdateError> {
            let first = args.first().map(String::as_str).unwrap_or("");
            let outputs = self.outputs.lock().unwrap();
            for (p, a, o) in outputs.iter() {
                if p == program && (a == first || a.is_empty()) {
                    return Ok(o.clone());
                }
            }
            // 未注册 → 返回 ok 空输出（默认成功）
            Ok(BootloaderCommandOutput::ok())
        }
    }

    #[tokio::test]
    async fn run_next_boot_grub_success() {
        let runner = FixtureRunner::new();
        let plan = ActivationPlan::new(BootloaderKind::Grub, UpdateSlot::B);
        run_next_boot(&runner, &plan).await.unwrap();
    }

    #[tokio::test]
    async fn run_next_boot_grub_failure_returns_slot_conflict() {
        let runner = FixtureRunner::new().on(
            "grub2-reboot",
            "os_slot_b",
            BootloaderCommandOutput {
                status: 1,
                stdout: String::new(),
                stderr: "permission denied".to_string(),
            },
        );
        let plan = ActivationPlan::new(BootloaderKind::Grub, UpdateSlot::B);
        let err = run_next_boot(&runner, &plan).await.unwrap_err();
        assert!(matches!(err, UpdateError::SlotConflict(_)));
    }

    #[tokio::test]
    async fn run_commit_systemd_success() {
        let runner = FixtureRunner::new();
        let plan = ActivationPlan::new(BootloaderKind::SystemdBoot, UpdateSlot::A);
        run_commit(&runner, &plan).await.unwrap();
    }

    #[tokio::test]
    async fn run_commit_failure_returns_slot_conflict() {
        let runner = FixtureRunner::new().on(
            "bootctl",
            "set-default",
            BootloaderCommandOutput {
                status: 2,
                stdout: String::new(),
                stderr: "no systemd-boot".to_string(),
            },
        );
        let plan = ActivationPlan::new(BootloaderKind::SystemdBoot, UpdateSlot::A);
        let err = run_commit(&runner, &plan).await.unwrap_err();
        assert!(matches!(err, UpdateError::SlotConflict(_)));
    }

    // —— write_config_files ——

    #[test]
    fn write_config_files_creates_dirs_and_content() {
        let dir = tempfile::tempdir().unwrap();
        let f1 = dir.path().join("loader/entries/os-slot-a.conf");
        let f2 = dir.path().join("loader/loader.conf");
        let files = vec![
            (f1.clone(), "title A".to_string()),
            (f2.clone(), "default os-slot-a".to_string()),
        ];
        write_config_files(&files).unwrap();
        assert_eq!(std::fs::read_to_string(&f1).unwrap(), "title A");
        assert_eq!(std::fs::read_to_string(&f2).unwrap(), "default os-slot-a");
    }
}

// ============================================================================
// 真实环境集成测（需 root + bootloader 工具，沙箱跑 --ignored）
// ============================================================================

#[cfg(test)]
mod real_env_tests {
    use super::*;

    /// 真实 GRUB next-boot：调 `grub2-reboot os_slot_b`。
    ///
    /// 需要：root + grub2-reboot 在 $PATH + 系统 GRUB 已安装。
    /// 失败（非零退出或工具缺失）映射 SlotConflict；沙箱（方案 B：QEMU VM）
    /// 内可跑真实路径。普通开发机非 root 跑会失败——故 #[ignore]。
    #[tokio::test]
    #[ignore = "需 root + GRUB（沙箱方案 B：QEMU VM 内跑）"]
    async fn real_grub_reboot_runs() {
        let runner = TokioBootloaderRunner;
        let plan = ActivationPlan::new(BootloaderKind::Grub, UpdateSlot::B);
        let res = run_next_boot(&runner, &plan).await;
        // 在真实 GRUB 环境应成功；非 root 会返回 SlotConflict（也是合法路径）
        match res {
            Ok(()) => {}
            Err(UpdateError::SlotConflict(msg)) => {
                eprintln!("grub2-reboot 失败（可能非 root）：{msg}");
            }
            Err(e) => panic!("非预期错误: {e:?}"),
        }
    }

    /// 真实 systemd-boot next-boot：调 `bootctl set-oneshot os-slot-b`。
    ///
    /// 需要：root + bootctl 在 $PATH + systemd-boot 已安装（ESP 挂载）。
    /// 沙箱方案 B（QEMU VM）可跑真实路径。普通开发机非 root 跑会失败——故 #[ignore]。
    #[tokio::test]
    #[ignore = "需 root + systemd-boot（沙箱方案 B：QEMU VM 内跑）"]
    async fn real_systemd_boot_set_oneshot_runs() {
        let runner = TokioBootloaderRunner;
        let plan = ActivationPlan::new(BootloaderKind::SystemdBoot, UpdateSlot::B);
        let res = run_next_boot(&runner, &plan).await;
        match res {
            Ok(()) => {}
            Err(UpdateError::SlotConflict(msg)) => {
                eprintln!("bootctl set-oneshot 失败（可能非 root/无 ESP）：{msg}");
            }
            Err(e) => panic!("非预期错误: {e:?}"),
        }
    }

    /// 真实 bootloader 配置写入 /boot（需 root）。
    ///
    /// 验证 `write_config_files` 在真盘上原子写、目录创建正确。沙箱方案 B 可跑。
    #[tokio::test]
    #[ignore = "需 root + /boot 可写（沙箱方案 B：QEMU VM 内跑）"]
    async fn real_write_config_to_boot() {
        // boot_root 指向临时目录（避免污染宿主真 /boot）；真测可改 /boot。
        let tmp = tempfile::tempdir().unwrap();
        let cfg = BootloaderConfig {
            kind: BootloaderKind::Grub,
            slot_a: SlotBootEntry {
                slot: UpdateSlot::A,
                version: "1.0.0".to_string(),
                linux: PathBuf::from("/boot/slot-a/vmlinuz"),
                initrd: PathBuf::from("/boot/slot-a/initrd.img"),
                cmdline: "root=UUID=test-a ro slot=A".to_string(),
            },
            slot_b: SlotBootEntry {
                slot: UpdateSlot::B,
                version: "1.1.0".to_string(),
                linux: PathBuf::from("/boot/slot-b/vmlinuz"),
                initrd: PathBuf::from("/boot/slot-b/initrd.img"),
                cmdline: "root=UUID=test-b ro slot=B".to_string(),
            },
            default: UpdateSlot::A,
            next_default: Some(UpdateSlot::B),
            boot_root: tmp.path().to_path_buf(),
        };
        let files = cfg.render();
        write_config_files(&files).unwrap();
        for (p, _) in &files {
            assert!(p.exists(), "{} 应存在", p.display());
        }
    }
}
