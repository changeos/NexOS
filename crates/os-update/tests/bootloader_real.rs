//! os-update bootloader 真实测：配置生成 + 命令构造 + activate_slot 编排 +
//! 真实 bootloader 工具可达性（#[ignore]）。
//!
//! 模块边界（见 `crates/os-update/src/bootloader.rs`）：
//! - 配置生成（`grub`/`systemd_boot` 模块）：纯函数，有独立单测；
//! - `BootloaderRunner` trait：执行 `grub2-reboot`/`bootctl set-default`；
//! - `ActivationPlan`：两阶段命令构造（next-boot 一次性 → commit 持久 default）；
//! - `run_next_boot`/`run_commit`：编排纯逻辑（调 runner）；
//! - `AbUpdateEngine::activate_slot`：真实编排入口（写配置 + run_next_boot）。
//!
//! 本文件覆盖前三项的**集成测**（默认跑，纯逻辑，不触盘不触网），
//! 外加真实工具可达性探测（`#[ignore]`，本机跑）。
//!
//! ## 红线
//! **绝不真改 boot 默认条目**——`bootctl set-default`/`grub-reboot` 真跑会改下次启动
//! （破坏性）。`#[ignore]` 测只做只读探测（`--version`/`status`/`list`）+ 命令构造验证。

#![cfg(test)]

use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use os_update::bootloader::{self, BootloaderCommandOutput, BootloaderRunner};
use os_update::{
    ActivationPlan, BootloaderConfig, BootloaderKind, SlotBootEntry, TokioBootloaderRunner,
    UpdateError, UpdateSlot,
};
use tempfile::tempdir;

// ============================================================================
// 测试夹具：构造典型 A/B 槽 bootloader 配置 + 记录型 runner
// ============================================================================

/// 构造一个 SlotBootEntry（kernel/initrd 路径置于给定 boot_root 下）。
fn mk_entry(slot: UpdateSlot, version: &str, boot_root: &std::path::Path) -> SlotBootEntry {
    let tag = match slot {
        UpdateSlot::A => 'a',
        UpdateSlot::B => 'b',
    };
    SlotBootEntry {
        slot,
        version: version.to_string(),
        linux: boot_root.join(format!("slot-{tag}/vmlinuz")),
        initrd: boot_root.join(format!("slot-{tag}/initrd.img")),
        // cmdline 含 root=UUID=... 供 GRUB extract_root 提取
        cmdline: format!("root=UUID=slot-{tag}-root ro slot={slot:?} quiet"),
    }
}

/// 构造典型 A/B 双槽 bootloader 配置（default=给定槽，next_default=None）。
fn sample_config(
    kind: BootloaderKind,
    default: UpdateSlot,
    boot_root: &std::path::Path,
) -> BootloaderConfig {
    BootloaderConfig {
        kind,
        slot_a: mk_entry(UpdateSlot::A, "1.0.0", boot_root),
        slot_b: mk_entry(UpdateSlot::B, "1.1.0", boot_root),
        default,
        next_default: None,
        boot_root: boot_root.to_path_buf(),
    }
}

/// 单条调用记录（program + args）。
type Call = (String, Vec<String>);
/// 单条预设输出（program + args 首元素 + 输出）。
type Fixture = (String, String, BootloaderCommandOutput);

/// 记录型 runner：捕获所有 `(program, args)` 调用 + 按 (program, args[0]) 分发预设输出。
///
/// 默认（未注册）返回成功空输出，便于"只关心调用记录"的编排测。
#[derive(Default, Clone)]
struct RecordingRunner {
    calls: std::sync::Arc<std::sync::Mutex<Vec<Call>>>,
    outputs: std::sync::Arc<std::sync::Mutex<Vec<Fixture>>>,
}

impl RecordingRunner {
    fn new() -> Self {
        Self::default()
    }

    /// 注册：当 `program == prog && args[0] == first` 时返回 `out`。
    fn on(self, prog: &str, first: &str, out: BootloaderCommandOutput) -> Self {
        self.outputs
            .lock()
            .unwrap()
            .push((prog.to_string(), first.to_string(), out));
        self
    }

    /// 取所有调用记录（program + args 快照）。
    fn calls(&self) -> Vec<(String, Vec<String>)> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl BootloaderRunner for RecordingRunner {
    async fn run(
        &self,
        program: &str,
        args: &[String],
    ) -> Result<BootloaderCommandOutput, UpdateError> {
        self.calls
            .lock()
            .unwrap()
            .push((program.to_string(), args.to_vec()));
        let first = args.first().map(String::as_str).unwrap_or("");
        let outputs = self.outputs.lock().unwrap();
        for (p, a, o) in outputs.iter() {
            if p == program && (a == first || a.is_empty()) {
                return Ok(o.clone());
            }
        }
        Ok(BootloaderCommandOutput::ok())
    }
}

// ============================================================================
// A. 配置生成测（默认跑，纯逻辑）
// ============================================================================

/// GRUB 配置生成：menuentry/set default/next-entry 结构语法正确。
#[test]
fn grub_config_has_menuentry_default_and_next_entry_structure() {
    let tmp = tempdir().unwrap();
    let mut cfg = sample_config(BootloaderKind::Grub, UpdateSlot::A, tmp.path());
    cfg.set_next_boot(UpdateSlot::B); // 模拟 activate_slot 设一次性 next-boot

    let rendered = cfg.render();
    assert_eq!(rendered.len(), 1, "GRUB 渲染应只产 1 个 grub.cfg");
    let (path, content) = &rendered[0];
    assert!(
        path.ends_with("grub/grub.cfg"),
        "GRUB 配置路径应以 grub/grub.cfg 结尾，实际: {}",
        path.display()
    );

    // 1. set default=<id>（持久 default = A）
    assert!(
        content.contains("set default=os_slot_a"),
        "缺 set default=os_slot_a（持久 default 行）"
    );
    // 2. 两槽 menuentry（含 --id + title + linux + initrd）
    for (slot, id) in [(UpdateSlot::A, "os_slot_a"), (UpdateSlot::B, "os_slot_b")] {
        assert!(
            content.contains(&format!("--id {id}")),
            "缺 menuentry --id {id}（{slot:?} 槽）"
        );
        let tag = match slot {
            UpdateSlot::A => 'a',
            UpdateSlot::B => 'b',
        };
        assert!(
            content.contains(&format!("slot-{tag}/vmlinuz")),
            "缺 {slot:?} 槽 linux 行"
        );
        assert!(
            content.contains(&format!("slot-{tag}/initrd.img")),
            "缺 {slot:?} 槽 initrd 行"
        );
    }
    // 3. next-entry 注释标记（GRUB 无原生 next-default 变量，由 grub2-reboot 维护）
    assert!(
        content.contains("next-boot oneshot"),
        "缺 next-boot oneshot 注释（next_default 标记）"
    );
    assert!(
        content.contains("grub2-reboot os_slot_b"),
        "next-default 注释应引用 grub2-reboot os_slot_b"
    );
    // 4. extract_root：cmdline 含 root=UUID=slot-b-root，应被提取到 menuentry set root=
    //    （注意：GRUB menuentry 的 linux 行格式为 `linux <path> root=<root> ro <cmdline>`，
    //     cmdline 本身含 root=，故 root 值应出现在内容里）
    assert!(
        content.contains("UUID=slot-a-root"),
        "extract_root 应从 cmdline 提取 root 值 (slot-a-root)"
    );
}

/// GRUB 配置：default 槽的 menuentry 应排在前面（GRUB 高亮默认）。
#[test]
fn grub_config_renders_default_slot_menuentry_first() {
    let tmp = tempdir().unwrap();
    let cfg = sample_config(BootloaderKind::Grub, UpdateSlot::B, tmp.path());

    let (_, content) = &cfg.render()[0];
    // default=B，故 os_slot_b 的 menuentry 应先出现
    let b_pos = content.find("--id os_slot_b").expect("应含 os_slot_b");
    let a_pos = content.find("--id os_slot_a").expect("应含 os_slot_a");
    assert!(
        b_pos < a_pos,
        "default 槽 (B) 的 menuentry 应排在 A 之前（GRUB 默认高亮）"
    );
}

/// systemd-boot 配置生成：loader.conf + 两槽 entry conf 语法正确。
#[test]
fn systemd_boot_config_has_loader_and_entries_structure() {
    let tmp = tempdir().unwrap();
    let mut cfg = sample_config(BootloaderKind::SystemdBoot, UpdateSlot::A, tmp.path());
    cfg.set_next_boot(UpdateSlot::B);

    let files = cfg.render();
    assert_eq!(files.len(), 3, "systemd-boot 应渲染 3 个文件");

    let by_path: std::collections::HashMap<&PathBuf, &String> =
        files.iter().map(|(p, c)| (p, c)).collect();

    // 1. 两槽 entry conf（Boot Loader Spec 类型#1）
    let entry_a = by_path
        .get(&tmp.path().join("loader/entries/os-slot-a.conf"))
        .expect("缺 os-slot-a.conf");
    assert!(entry_a.contains("title   OS 1.0.0 (slot A)"));
    assert!(entry_a.contains("version 1.0.0"));
    assert!(entry_a.contains("linux   "));
    assert!(entry_a.contains("initrd  "));
    assert!(entry_a.contains("options "));

    let entry_b = by_path
        .get(&tmp.path().join("loader/entries/os-slot-b.conf"))
        .expect("缺 os-slot-b.conf");
    assert!(entry_b.contains("title   OS 1.1.0 (slot B)"));

    // 2. loader.conf（default + timeout + next-boot 注释）
    let loader = by_path
        .get(&tmp.path().join("loader/loader.conf"))
        .expect("缺 loader.conf");
    assert!(
        loader.contains("default os-slot-a"),
        "loader.conf 应含 default os-slot-a（持久 default）"
    );
    assert!(loader.contains("timeout 5"), "loader.conf 应含 timeout 5");
    assert!(
        loader.contains("next-boot oneshot"),
        "loader.conf 应含 next-boot oneshot 注释（next_default 标记）"
    );
    assert!(
        loader.contains("bootctl set-oneshot os-slot-b"),
        "loader.conf next-default 注释应引用 bootctl set-oneshot os-slot-b"
    );
}

/// systemd-boot entry_id / entry_conf_filename 约定稳定（bootctl set-default 引用）。
#[test]
fn systemd_boot_entry_ids_are_stable_for_bootctl_references() {
    assert_eq!(
        bootloader::systemd_boot::entry_id(UpdateSlot::A),
        "os-slot-a"
    );
    assert_eq!(
        bootloader::systemd_boot::entry_id(UpdateSlot::B),
        "os-slot-b"
    );
    assert_eq!(
        bootloader::systemd_boot::entry_conf_filename(UpdateSlot::A),
        "os-slot-a.conf"
    );
    assert_eq!(
        bootloader::systemd_boot::entry_conf_filename(UpdateSlot::B),
        "os-slot-b.conf"
    );
    // GRUB 侧（grub2-reboot 引用）
    assert_eq!(bootloader::grub::menuentry_id(UpdateSlot::A), "os_slot_a");
    assert_eq!(bootloader::grub::menuentry_id(UpdateSlot::B), "os_slot_b");
}

// ============================================================================
// B. BootloaderRunner 命令构造测（默认跑，纯逻辑）
// ============================================================================

/// next-boot 阶段命令构造：grub2-reboot <entry> / bootctl set-oneshot <id>。
#[test]
fn next_boot_command_construction_matches_tool_arg_conventions() {
    // GRUB：grub2-reboot os_slot_b（单参数 entry id）
    let grub_plan = ActivationPlan::new(BootloaderKind::Grub, UpdateSlot::B);
    let (prog, args) = grub_plan.next_boot_command();
    assert_eq!(prog, "grub2-reboot");
    assert_eq!(args, vec!["os_slot_b".to_string()]);
    // 断言 argv 长度 = 1（grub2-reboot 只接受 entry id 单参）
    assert_eq!(args.len(), 1, "grub2-reboot 应只接 1 个参数 (entry id)");

    // systemd-boot：bootctl set-oneshot os-slot-a（子命令 + entry id）
    let sd_plan = ActivationPlan::new(BootloaderKind::SystemdBoot, UpdateSlot::A);
    let (prog, args) = sd_plan.next_boot_command();
    assert_eq!(prog, "bootctl");
    assert_eq!(
        args,
        vec!["set-oneshot".to_string(), "os-slot-a".to_string()]
    );
    assert_eq!(
        args.len(),
        2,
        "bootctl set-oneshot 应接 2 个参数 (子命令 + id)"
    );
}

/// commit 阶段命令构造：grub2-set-default <entry> / bootctl set-default <id>。
#[test]
fn commit_command_construction_matches_tool_arg_conventions() {
    // GRUB：grub2-set-default os_slot_a（持久 default）
    let grub_plan = ActivationPlan::new(BootloaderKind::Grub, UpdateSlot::A);
    let (prog, args) = grub_plan.commit_command();
    assert_eq!(prog, "grub2-set-default");
    assert_eq!(args, vec!["os_slot_a".to_string()]);

    // systemd-boot：bootctl set-default os-slot-b（持久 default）
    let sd_plan = ActivationPlan::new(BootloaderKind::SystemdBoot, UpdateSlot::B);
    let (prog, args) = sd_plan.commit_command();
    assert_eq!(prog, "bootctl");
    assert_eq!(
        args,
        vec!["set-default".to_string(), "os-slot-b".to_string()]
    );
}

/// run_next_boot 实际派发：fixture runner 捕获到的 argv 与 plan.next_boot_command() 一致。
#[tokio::test]
async fn run_next_boot_dispatches_expected_command_to_runner() {
    let runner = RecordingRunner::new();
    let plan = ActivationPlan::new(BootloaderKind::Grub, UpdateSlot::B);
    let (expected_prog, expected_args) = plan.next_boot_command();

    bootloader::run_next_boot(&runner, &plan).await.unwrap();

    let calls = runner.calls();
    assert_eq!(calls.len(), 1, "run_next_boot 应恰好调 1 次 runner");
    assert_eq!(calls[0].0, expected_prog);
    assert_eq!(calls[0].1, expected_args);
    // 显式断言完整 argv
    assert_eq!(
        calls[0],
        ("grub2-reboot".to_string(), vec!["os_slot_b".to_string()])
    );
}

/// run_commit 实际派发：fixture runner 捕获到的 argv 与 plan.commit_command() 一致。
#[tokio::test]
async fn run_commit_dispatches_expected_command_to_runner() {
    let runner = RecordingRunner::new();
    let plan = ActivationPlan::new(BootloaderKind::SystemdBoot, UpdateSlot::A);
    let (expected_prog, expected_args) = plan.commit_command();

    bootloader::run_commit(&runner, &plan).await.unwrap();

    let calls = runner.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, expected_prog);
    assert_eq!(calls[0].1, expected_args);
    assert_eq!(
        calls[0],
        (
            "bootctl".to_string(),
            vec!["set-default".to_string(), "os-slot-a".to_string()]
        )
    );
}

// ============================================================================
// C. 错误处理测（默认跑，纯逻辑）
// ============================================================================

/// run_next_boot：runner 返回非零退出码时，错误映射为 SlotConflict 并保留 stderr。
#[tokio::test]
async fn run_next_boot_nonzero_exit_propagates_slot_conflict() {
    let runner = RecordingRunner::new().on(
        "grub2-reboot",
        "os_slot_b",
        BootloaderCommandOutput {
            status: 1,
            stdout: String::new(),
            stderr: "permission denied: need root".to_string(),
        },
    );
    let plan = ActivationPlan::new(BootloaderKind::Grub, UpdateSlot::B);
    let err = bootloader::run_next_boot(&runner, &plan).await.unwrap_err();
    match err {
        UpdateError::SlotConflict(msg) => {
            assert!(
                msg.contains("grub2-reboot"),
                "错误信息应含程序名 grub2-reboot，实际: {msg}"
            );
            assert!(
                msg.contains("permission denied"),
                "错误信息应保留 stderr，实际: {msg}"
            );
        }
        other => panic!("应为 SlotConflict，实际: {other:?}"),
    }
}

/// run_commit：runner 返回非零退出码时，错误映射为 SlotConflict。
#[tokio::test]
async fn run_commit_nonzero_exit_propagates_slot_conflict() {
    let runner = RecordingRunner::new().on(
        "bootctl",
        "set-default",
        BootloaderCommandOutput {
            status: 1,
            stdout: String::new(),
            stderr: "ESP not mounted".to_string(),
        },
    );
    let plan = ActivationPlan::new(BootloaderKind::SystemdBoot, UpdateSlot::A);
    let err = bootloader::run_commit(&runner, &plan).await.unwrap_err();
    assert!(
        matches!(err, UpdateError::SlotConflict(ref m) if m.contains("bootctl")),
        "commit 失败应映射 SlotConflict 含 bootctl，实际: {err:?}"
    );
}

/// TokioBootloaderRunner：spawn 不存在的程序应返回 SlotConflict（不 panic）。
#[tokio::test]
async fn tokio_runner_missing_binary_returns_slot_conflict_not_panic() {
    let runner = TokioBootloaderRunner;
    // 一个保证不存在的程序名
    let res = runner.run("this-binary-does-not-exist-xyz123", &[]).await;
    assert!(
        res.is_err(),
        "spawn 不存在的程序应返回 Err（而非 Ok 带 -1）"
    );
    match res {
        Err(UpdateError::SlotConflict(msg)) => {
            assert!(
                msg.contains("this-binary-does-not-exist-xyz123"),
                "错误信息应含程序名，实际: {msg}"
            );
        }
        other => panic!("应为 SlotConflict，实际: {other:?}"),
    }
}

// ============================================================================
// D. BootloaderConfig 状态机 + render 一致性（默认跑）
// ============================================================================

/// effective_next_boot：next_default 优先于 default；set_default 清 next_default。
#[test]
fn config_state_machine_effective_next_boot_and_set_default() {
    let tmp = tempdir().unwrap();
    let mut cfg = sample_config(BootloaderKind::SystemdBoot, UpdateSlot::A, tmp.path());

    // 初始：default=A，next_default=None → effective=A
    assert_eq!(cfg.effective_next_boot(), UpdateSlot::A);

    // 设 next_boot=B → effective=B（持久 default 仍 A）
    cfg.set_next_boot(UpdateSlot::B);
    assert_eq!(cfg.effective_next_boot(), UpdateSlot::B);
    assert_eq!(
        cfg.default,
        UpdateSlot::A,
        "set_next_boot 不应改持久 default"
    );

    // 模拟探活通过 → commit：set_default=B 应清 next_default
    cfg.set_default(UpdateSlot::B);
    assert_eq!(cfg.default, UpdateSlot::B);
    assert_eq!(cfg.next_default, None, "set_default 应清掉 next_default");
    assert_eq!(cfg.effective_next_boot(), UpdateSlot::B);
}

/// render() 按 kind 分发：GRUB=1 文件，systemd-boot=3 文件；路径基于 boot_root。
#[test]
fn render_dispatches_by_kind_with_correct_file_count() {
    let tmp = tempdir().unwrap();
    // GRUB
    let grub = sample_config(BootloaderKind::Grub, UpdateSlot::A, tmp.path());
    let grub_files = grub.render();
    assert_eq!(grub_files.len(), 1);
    assert!(grub_files[0].0.starts_with(tmp.path()));

    // systemd-boot
    let sd = sample_config(BootloaderKind::SystemdBoot, UpdateSlot::B, tmp.path());
    let sd_files = sd.render();
    assert_eq!(sd_files.len(), 3);
    for (p, _) in &sd_files {
        assert!(
            p.starts_with(tmp.path()),
            "systemd-boot 文件路径应基于 boot_root"
        );
    }
}

/// write_config_files：原子写 + 创建子目录 + 内容正确。
#[test]
fn write_config_files_creates_dirs_and_writes_atomically() {
    let tmp = tempdir().unwrap();
    let root = tmp.path();
    let files = vec![
        (
            root.join("loader/entries/os-slot-a.conf"),
            "title OS A\n".to_string(),
        ),
        (
            root.join("grub/grub.cfg"),
            "set default=os_slot_a\n".to_string(),
        ),
    ];
    bootloader::write_config_files(&files).unwrap();
    // 内容校验
    assert_eq!(
        std::fs::read_to_string(root.join("loader/entries/os-slot-a.conf")).unwrap(),
        "title OS A\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("grub/grub.cfg")).unwrap(),
        "set default=os_slot_a\n"
    );
    // 原子写：.new 临时文件应已 cleanup（rename 后不存在）
    assert!(
        !root.join("loader/entries/os-slot-a.conf.new").exists(),
        "原子写后 .new 临时文件应已 rename 掉"
    );
}

// ============================================================================
// E. 真实 bootloader 工具可达性（#[ignore]，本机跑）
// ============================================================================
//
// 红线：以下测试**只做只读探测**（--version / status / list）+ 命令构造验证。
// **严禁**真跑 bootctl set-default / grub-reboot（会改下次启动，破坏性）。
//
// 用 TokioBootloaderRunner 真实 spawn 子进程；工具缺失则优雅 SKIP。

/// 工具可用性探测：which/版本，返回 (program, version_line) 或 None。
fn probe_tool(name: &str) -> Option<String> {
    let out = std::process::Command::new(name)
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    Some(line.lines().next().unwrap_or("").to_string())
}

/// bootctl 可达性：`bootctl --version`（systemd 提供）。
///
/// 本机环境：systemd 259 装了，但 Ubuntu 的 systemd 包**不含 bootctl 二进制**
/// （需额外装 systemd-boot 包）。此测验证 TokioBootloaderRunner 真实 spawn +
/// bootctl 存在性；缺失则 SKIP。
#[tokio::test]
#[ignore = "真实工具探测：需本机 bootctl（systemd-boot 包）"]
async fn real_bootctl_version_reachable() {
    let which = probe_tool("bootctl");
    if which.is_none() {
        eprintln!("SKIP: bootctl 不在 $PATH（本机未装 systemd-boot 包）");
        return;
    }
    eprintln!("bootctl 探测: {}", which.unwrap());

    // 用 TokioBootloaderRunner 真实 spawn bootctl --version（只读，不改默认）
    let runner = TokioBootloaderRunner;
    let out = runner
        .run("bootctl", &["--version".to_string()])
        .await
        .expect("bootctl --version 不应 spawn 失败（已探测存在）");
    assert_eq!(
        out.status,
        0,
        "bootctl --version 应退出码 0，实际 {} (stderr: {})",
        out.status,
        out.stderr.trim()
    );
    assert!(
        out.stdout.contains("systemd") || out.stdout.contains("bootctl"),
        "bootctl --version stdout 应含 systemd/bootctl 标识，实际: {}",
        out.stdout.trim()
    );
    eprintln!("bootctl --version OK: {}", out.stdout.trim());
}

/// bootctl list（只读列出当前 boot entries；若支持）。
///
/// 注意：`bootctl list` 是只读操作（不改默认/oneshot），安全。
/// 若 bootctl 不存在或 list 子命令不支持（旧版），SKIP。
#[tokio::test]
#[ignore = "真实工具探测：需本机 bootctl + ESP"]
async fn real_bootctl_list_is_readonly_and_works() {
    if probe_tool("bootctl").is_none() {
        eprintln!("SKIP: bootctl 不在 $PATH");
        return;
    }
    let runner = TokioBootloaderRunner;
    // bootctl list 是只读列出 entries（不改 default/oneshot）
    let out = runner
        .run("bootctl", &["list".to_string()])
        .await
        .expect("bootctl spawn 不应失败");
    // list 可能因无 ESP 返回非零（合法），但不应 spawn 失败
    eprintln!(
        "bootctl list 退出码 {}，stdout 前 200 字:\n{}",
        out.status,
        out.stdout.chars().take(200).collect::<String>()
    );
    // 不强断言 status==0（无 ESP 时 bootctl list 会非零）；只验证 runner 可 spawn
}

/// grub-reboot / grub2-reboot 可达性：which + 版本探测（不真跑切换）。
///
/// 本机：Ubuntu 提供 `grub-reboot`（GRUB 2.14），无 `grub2-reboot`（Fedora/RHEL 命名）。
/// 此测只做存在性 + 版本探测；**不真跑 grub-reboot <entry>**（会改下次启动）。
#[tokio::test]
#[ignore = "真实工具探测：需本机 grub"]
async fn real_grub_reboot_reachable_probe_only() {
    // 探测 grub2-reboot（Fedora/RHEL 命名）与 grub-reboot（Debian/Ubuntu 命名）
    let grub2 = probe_tool("grub2-reboot");
    let grub = probe_tool("grub-reboot");
    if grub2.is_none() && grub.is_none() {
        eprintln!("SKIP: 既无 grub2-reboot 也无 grub-reboot");
        return;
    }
    if let Some(v) = &grub2 {
        eprintln!("grub2-reboot 探测: {v}");
    }
    if let Some(v) = &grub {
        eprintln!("grub-reboot 探测: {v}");
    }

    // 命令构造验证（不真跑）：ActivationPlan 构造的 argv 应与 grub2-reboot 约定一致
    let plan = ActivationPlan::new(BootloaderKind::Grub, UpdateSlot::B);
    let (prog, args) = plan.next_boot_command();
    assert_eq!(prog, "grub2-reboot");
    assert_eq!(args, vec!["os_slot_b".to_string()]);
    eprintln!(
        "命令构造 OK: {prog} {} (未真跑——避免改下次启动)",
        args.join(" ")
    );

    // 红线复查：此处**不**调 bootloader::run_next_boot(&TokioBootloaderRunner, &plan)
    // （那会真改下次启动）。只验证命令构造正确性。
}

/// 真实 runner spawn 真实只读命令：`true`（/bin/true 一定存在，退出码 0）。
///
/// 验证 TokioBootloaderRunner 的 spawn/stdout 捕获/退出码解析在真实环境工作。
#[tokio::test]
#[ignore = "真实 spawn 探测：验证 TokioBootloaderRunner 子进程机制"]
async fn real_tokio_runner_spawns_true_and_captures_exit_zero() {
    let runner = TokioBootloaderRunner;
    let out = runner.run("true", &[]).await.expect("spawn true 不应失败");
    assert_eq!(out.status, 0, "/bin/true 应退出码 0");
}

/// 真实 runner spawn 真实非零命令：`false`（退出码 1）。
///
/// 验证 TokioBootloaderRunner 正确解析非零退出码（不 panic）。
#[tokio::test]
#[ignore = "真实 spawn 探测：验证 TokioBootloaderRunner 非零退出码解析"]
async fn real_tokio_runner_spawns_false_and_captures_exit_one() {
    let runner = TokioBootloaderRunner;
    let out = runner
        .run("false", &[])
        .await
        .expect("spawn false 不应失败");
    assert_eq!(out.status, 1, "/bin/false 应退出码 1");
}
