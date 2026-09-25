//! 场景9：provision PXE 自举链 —— 集成测试
//!
//! 覆盖：
//! - PhaseMachine 完整生命周期 (SystemInit -> FileTransfer -> ExcludeSensitive -> FirstBoot)
//! - S3.19 ExcludeRules 敏感过滤（默认 9 类别全覆盖，partition 正确分流）
//! - CheckpointPolicy 断点续传决策（全新/已完成/中断恢复）
//! - PxeConfigBuilder 产物正确性（iPXE 脚本、pxelinux.cfg、DHCP、TFTP 清单，3 种 BootMode）
//! - MockProvisioner PXE boot -> init_system 链
//! - MockMigrationEngine plan/execute/resume 链 + 排除键生成
//! - ExcludeOutcome 安全审计：不存储敏感路径/密钥本体

use std::net::{IpAddr, Ipv4Addr};

use os_core::{DatasetId, NodeId};
use os_provision::checkpoint::{
    CheckpointPolicy, ExcludeOutcome, MigrationCheckpoint, ResumeDecision, TransferredFile,
};
use os_provision::exclude::{default_excludes, ExcludeCategory, ExcludeRules, FilterOutcome};
use os_provision::migration::{MigrationEngine, MigrationPlan, MigrationStatus};
use os_provision::phase::{MigrationPhase, PhaseMachine, PhaseTransition};
use os_provision::provision::{ProvisionConfig, ProvisionStatus, ProvisionTarget, Provisioner};
use os_provision::pxe::{BootMode, PxeBootParams, PxeConfigBuilder};
use os_provision::MockMigrationEngine;
use os_provision::MockProvisioner;

// ============================================================================
// 辅助构造
// ============================================================================

fn sample_target() -> ProvisionTarget {
    ProvisionTarget {
        mac: "aa:bb:cc:dd:ee:ff".into(),
        ip: Some("10.0.0.5".into()),
        arch: "x86_64".into(),
        endpoint: "10.0.0.5:8443".into(),
    }
}

fn sample_config() -> ProvisionConfig {
    ProvisionConfig {
        base_image: "/img/base.squashfs".into(),
        root_password_hash: "$6$rounds=4096$salt$hash".into(),
        zfs_pool_disks: vec!["/dev/sda".into(), "/dev/sdb".into()],
        network_config: serde_json::json!({"hostname": "os-node-1"}),
    }
}

fn sample_pxe_params(boot_mode: BootMode) -> PxeBootParams {
    PxeBootParams {
        http_repo: "http://10.0.0.1:8080/provision".into(),
        kernel_path: "vmlinuz".into(),
        initramfs_path: "initrd.img".into(),
        base_image_path: "base.squashfs".into(),
        install_disk: "/dev/sda".into(),
        tftp_server: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
        boot_mode,
    }
}

// ============================================================================
// 1. PhaseMachine 完整生命周期
// ============================================================================

#[test]
fn phase_machine_full_lifecycle_four_advances() {
    let mut m = PhaseMachine::new();
    assert_eq!(m.current(), MigrationPhase::SystemInit);
    assert!(!m.is_finished());
    assert!(m.completed().is_empty());

    // Advance 1: SystemInit -> FileTransfer
    match m.advance() {
        PhaseTransition::Advance { completed, next } => {
            assert_eq!(completed, MigrationPhase::SystemInit);
            assert_eq!(next, Some(MigrationPhase::FileTransfer));
        }
        other => panic!("Expected Advance, got {:?}", other),
    }
    assert_eq!(m.current(), MigrationPhase::FileTransfer);
    assert_eq!(m.completed().len(), 1);

    // Advance 2: FileTransfer -> ExcludeSensitive
    match m.advance() {
        PhaseTransition::Advance { completed, next } => {
            assert_eq!(completed, MigrationPhase::FileTransfer);
            assert_eq!(next, Some(MigrationPhase::ExcludeSensitive));
        }
        other => panic!("Expected Advance, got {:?}", other),
    }
    assert_eq!(m.current(), MigrationPhase::ExcludeSensitive);

    // Advance 3: ExcludeSensitive -> FirstBoot (security-sensitive phase completed)
    assert!(m.current().is_security_sensitive());
    match m.advance() {
        PhaseTransition::Advance { completed, next } => {
            assert_eq!(completed, MigrationPhase::ExcludeSensitive);
            assert_eq!(next, Some(MigrationPhase::FirstBoot));
        }
        other => panic!("Expected Advance, got {:?}", other),
    }

    // Advance 4: FirstBoot -> finished (next = None)
    match m.advance() {
        PhaseTransition::Advance { completed, next } => {
            assert_eq!(completed, MigrationPhase::FirstBoot);
            assert!(next.is_none(), "Last advance should have next=None");
        }
        other => panic!("Expected Advance, got {:?}", other),
    }
    assert!(m.is_finished());
    assert_eq!(m.completed().len(), 4);
}

#[test]
fn phase_machine_resume_from_checkpoint() {
    let mut m = PhaseMachine::resume_from(
        vec![MigrationPhase::SystemInit, MigrationPhase::FileTransfer],
        MigrationPhase::ExcludeSensitive,
    );
    assert_eq!(m.current(), MigrationPhase::ExcludeSensitive);
    assert_eq!(m.completed().len(), 2);
    assert!(!m.is_finished());

    // Two more advances to finish
    assert!(matches!(m.advance(), PhaseTransition::Advance { .. }));
    assert!(matches!(m.advance(), PhaseTransition::Advance { .. }));
    assert!(m.is_finished());
}

#[test]
fn phase_machine_fail_does_not_corrupt_completed() {
    let mut m = PhaseMachine::new();
    let _ = m.advance(); // SystemInit -> FileTransfer
    let t = m.fail("disk I/O error");
    match t {
        PhaseTransition::Failed { phase, reason } => {
            assert_eq!(phase, MigrationPhase::FileTransfer);
            assert!(reason.contains("disk I/O error"));
        }
        other => panic!("Expected Failed, got {:?}", other),
    }
    // completed stays at 1 (only SystemInit)
    assert_eq!(m.completed().len(), 1);
    assert_eq!(m.current(), MigrationPhase::FileTransfer);
}

#[test]
fn phase_machine_exclude_sensitive_cannot_be_skipped() {
    // Use resume_from to construct an artificial state:
    // completed = [SystemInit, FileTransfer], current = FirstBoot (ExcludeSensitive skipped)
    let mut m = PhaseMachine::resume_from(
        vec![MigrationPhase::SystemInit, MigrationPhase::FileTransfer],
        MigrationPhase::FirstBoot,
    );
    let t = m.advance();
    match t {
        PhaseTransition::Failed { phase, reason } => {
            assert_eq!(phase, MigrationPhase::FirstBoot);
            assert!(reason.contains("ExcludeSensitive"));
        }
        other => panic!(
            "Expected Failed for skipping ExcludeSensitive, got {:?}",
            other
        ),
    }
    // completed should NOT have been polluted (pop rollback)
    assert_eq!(m.completed().len(), 2);
}

// ============================================================================
// 2. S3.19 ExcludeRules 敏感过滤
// ============================================================================

#[test]
fn default_excludes_cover_all_nine_categories() {
    let rules = ExcludeRules::defaults();
    let default_rules = default_excludes();
    assert!(!default_rules.is_empty());

    let cats: Vec<ExcludeCategory> = default_rules.iter().map(|r| r.category).collect();
    // default_excludes() covers 8 concrete categories; Other is a custom catch-all
    let expected_cats = [
        ExcludeCategory::SystemCredential,
        ExcludeCategory::TlsPrivateKey,
        ExcludeCategory::SshPrivateKey,
        ExcludeCategory::SmbCredential,
        ExcludeCategory::DatabasePassword,
        ExcludeCategory::JwtTotpSecret,
        ExcludeCategory::WalletKey,
        ExcludeCategory::ClusterSecret,
    ];
    for need in &expected_cats {
        assert!(
            cats.contains(need),
            "Default excludes missing category {:?}",
            need
        );
    }
    assert!(rules.len() >= 15, "Should have substantial default rules");
}

#[test]
fn exclude_rules_partition_sensitive_vs_safe() {
    let rules = ExcludeRules::defaults();
    let entries: Vec<&str> = vec![
        "/etc/hostname",
        "/etc/shadow",
        "/etc/os/config.toml",
        "/etc/ssl/private/server.key",
        "/etc/ssh/ssh_host_rsa_key",
        "/etc/os/jwt-signing.key",
        "/etc/samba/smb.conf",
        "/etc/samba/smbpasswd",
        "/var/lib/os/wallet/keystore/btc.json",
        "/etc/os/cluster/raft-key",
        "/home/alice/.ssh/known_hosts",
    ];
    let (transfer, excluded) = rules.partition(entries.iter().copied());

    // Safe entries should transfer
    assert!(transfer.contains(&"/etc/hostname"));
    assert!(transfer.contains(&"/etc/os/config.toml"));
    assert!(transfer.contains(&"/etc/samba/smb.conf"));
    assert!(transfer.contains(&"/home/alice/.ssh/known_hosts"));

    // Sensitive entries should be excluded
    let excluded_paths: Vec<&str> = excluded.iter().map(|(p, _)| *p).collect();
    assert!(excluded_paths.contains(&"/etc/shadow"));
    assert!(excluded_paths.contains(&"/etc/ssl/private/server.key"));
    assert!(excluded_paths.contains(&"/etc/ssh/ssh_host_rsa_key"));
    assert!(excluded_paths.contains(&"/etc/os/jwt-signing.key"));
    assert!(excluded_paths.contains(&"/etc/samba/smbpasswd"));
    assert!(excluded_paths.contains(&"/var/lib/os/wallet/keystore/btc.json"));
    assert!(excluded_paths.contains(&"/etc/os/cluster/raft-key"));

    // Verify excluded entries have proper categories
    let shadow_rule = excluded.iter().find(|(p, _)| *p == "/etc/shadow").unwrap();
    assert_eq!(shadow_rule.1.category, ExcludeCategory::SystemCredential);
    let wallet_rule = excluded
        .iter()
        .find(|(p, _)| *p == "/var/lib/os/wallet/keystore/btc.json")
        .unwrap();
    assert_eq!(wallet_rule.1.category, ExcludeCategory::WalletKey);
}

#[test]
fn exclude_rules_evaluate_single_entry() {
    let rules = ExcludeRules::defaults();
    assert!(matches!(
        rules.evaluate("/etc/shadow"),
        FilterOutcome::Excluded { .. }
    ));
    assert_eq!(rules.evaluate("/etc/hostname"), FilterOutcome::Transfer);
}

// ============================================================================
// 3. Checkpoint resume
// ============================================================================

#[test]
fn checkpoint_fresh_migration_starts_from_system_init() {
    let policy = CheckpointPolicy;
    let all_datasets = vec![DatasetId::new("tank/media"), DatasetId::new("tank/photos")];
    let pending = vec!["/etc/os/config.toml".to_string()];
    let rules = ExcludeRules::defaults();

    let d = policy.decide_resume(None, MigrationPhase::all(), &all_datasets, &pending, &rules);
    match d {
        ResumeDecision::ResumeFromPhase {
            phase,
            remaining_datasets,
            remaining_excludes,
        } => {
            assert_eq!(phase, MigrationPhase::SystemInit);
            assert_eq!(remaining_datasets.len(), 2);
            assert_eq!(remaining_excludes.len(), 1);
        }
        _ => panic!("Fresh migration should resume from SystemInit"),
    }
}

#[test]
fn checkpoint_finished_when_all_phases_done() {
    let mut cp = MigrationCheckpoint::initial("plan-1");
    cp.completed_phases = MigrationPhase::all().to_vec();
    cp.current_phase = MigrationPhase::FirstBoot;

    let policy = CheckpointPolicy;
    let rules = ExcludeRules::defaults();
    let d = policy.decide_resume(Some(&cp), MigrationPhase::all(), &[], &[], &rules);
    assert_eq!(d, ResumeDecision::Finished);
}

#[test]
fn checkpoint_resume_file_transfer_skips_transmitted() {
    let mut cp = MigrationCheckpoint::initial("plan-2");
    cp.completed_phases = vec![MigrationPhase::SystemInit];
    cp.current_phase = MigrationPhase::FileTransfer;
    cp.transferred
        .push(TransferredFile::new("tank/media", 1024 * 1024));

    let all_datasets = vec![
        DatasetId::new("tank/media"),
        DatasetId::new("tank/photos"),
        DatasetId::new("tank/docs"),
    ];
    let policy = CheckpointPolicy;
    let rules = ExcludeRules::defaults();
    let d = policy.decide_resume(Some(&cp), MigrationPhase::all(), &all_datasets, &[], &rules);

    match d {
        ResumeDecision::ResumeFromPhase {
            phase,
            remaining_datasets,
            ..
        } => {
            assert_eq!(phase, MigrationPhase::FileTransfer);
            let names: Vec<&str> = remaining_datasets.iter().map(|ds| ds.as_str()).collect();
            assert_eq!(names, vec!["tank/photos", "tank/docs"]);
        }
        _ => panic!("Should resume from FileTransfer"),
    }
}

#[test]
fn checkpoint_resume_exclude_sensitive_returns_pending() {
    let mut cp = MigrationCheckpoint::initial("plan-3");
    cp.completed_phases = vec![MigrationPhase::SystemInit, MigrationPhase::FileTransfer];
    cp.current_phase = MigrationPhase::ExcludeSensitive;

    let pending = vec![
        "/etc/shadow".to_string(),
        "/etc/hostname".to_string(),
        "/etc/os/jwt-signing.key".to_string(),
    ];
    let policy = CheckpointPolicy;
    let rules = ExcludeRules::defaults();
    let d = policy.decide_resume(Some(&cp), MigrationPhase::all(), &[], &pending, &rules);

    match d {
        ResumeDecision::ResumeFromPhase {
            phase,
            remaining_datasets,
            remaining_excludes,
        } => {
            assert_eq!(phase, MigrationPhase::ExcludeSensitive);
            assert!(remaining_datasets.is_empty());
            assert_eq!(remaining_excludes.len(), 3);
        }
        _ => panic!("Should resume from ExcludeSensitive"),
    }
}

#[test]
fn checkpoint_serialization_roundtrip() {
    let cp = MigrationCheckpoint::initial("plan-serial");
    let json = serde_json::to_string(&cp).expect("serialize");
    let back: MigrationCheckpoint = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.plan_id, "plan-serial");
    assert_eq!(back.current_phase, MigrationPhase::SystemInit);
}

#[test]
fn exclude_outcome_does_not_store_sensitive_bodies() {
    // ExcludeOutcome should only store counts + category names, never actual key data
    let o = ExcludeOutcome::default();
    let json = serde_json::to_string(&o).unwrap();
    assert!(!json.contains("shadow"));
    assert!(!json.contains("password"));
    assert!(!json.contains("key"));

    // Also verify from_partition
    let rules = ExcludeRules::defaults();
    let entries = ["/etc/shadow", "/etc/os/jwt-signing.key"];
    let (t, e) = rules.partition(entries.iter().copied());
    let outcome = ExcludeOutcome::from_partition(&t, &e);
    let json2 = serde_json::to_string(&outcome).unwrap();
    // hit_categories contains Debug-formatted category names, NOT the actual file paths
    assert!(!json2.contains("/etc/shadow"));
    assert!(!json2.contains("jwt-signing.key"));
}

// ============================================================================
// 4. PXE 配置生成正确性
// ============================================================================

#[test]
fn pxe_bios_artifacts_complete() {
    let params = sample_pxe_params(BootMode::Bios);
    let artifacts = PxeConfigBuilder::build(&params);

    // DHCP config
    assert_eq!(artifacts.dhcp.boot_filename, "pxelinux.0");
    assert_eq!(artifacts.dhcp.next_server, "10.0.0.1");

    // iPXE script
    let ipxe = artifacts
        .find_file("bootstrap.ipxe")
        .expect("bootstrap.ipxe must exist");
    assert!(ipxe.content.starts_with("#!ipxe\n"));
    assert!(ipxe
        .content
        .contains("kernel http://10.0.0.1:8080/provision/vmlinuz"));
    assert!(ipxe
        .content
        .contains("base_image=http://10.0.0.1:8080/provision/base.squashfs"));
    assert!(ipxe.content.contains("install_disk=/dev/sda"));
    assert!(ipxe
        .content
        .contains("initrd http://10.0.0.1:8080/provision/initrd.img"));
    assert!(ipxe.content.ends_with("boot\n"));

    // pxelinux.cfg/default
    let pxecfg = artifacts
        .find_file("pxelinux.cfg/default")
        .expect("pxelinux.cfg/default must exist");
    assert!(pxecfg.content.contains("DEFAULT ipxe"));
    assert!(pxecfg.content.contains("PROMPT 0"));
    assert!(pxecfg.content.contains("TIMEOUT 10"));
    assert!(pxecfg.content.contains("KERNEL undionly.kpxe"));

    // TFTP manifest: BIOS mode has pxelinux.0 + ldlinux.c32 + undionly.kpxe + bootstrap.ipxe
    let tftp_names: Vec<&str> = artifacts
        .tftp_manifest
        .iter()
        .map(|e| e.rel_path.as_str())
        .collect();
    assert!(tftp_names.contains(&"undionly.kpxe"));
    assert!(tftp_names.contains(&"pxelinux.0"));
    assert!(tftp_names.contains(&"ldlinux.c32"));
    assert!(tftp_names.contains(&"bootstrap.ipxe"));

    // Binary flags
    let pxelinux_entry = artifacts
        .tftp_manifest
        .iter()
        .find(|e| e.rel_path == "pxelinux.0")
        .unwrap();
    assert!(pxelinux_entry.is_binary);
    let ipxe_entry = artifacts
        .tftp_manifest
        .iter()
        .find(|e| e.rel_path == "bootstrap.ipxe")
        .unwrap();
    assert!(!ipxe_entry.is_binary);
}

#[test]
fn pxe_uefi_artifacts_no_pxelinux() {
    let params = sample_pxe_params(BootMode::Uefi);
    let artifacts = PxeConfigBuilder::build(&params);

    assert_eq!(artifacts.dhcp.boot_filename, "ipxe.efi");
    assert_eq!(artifacts.dhcp.next_server, "10.0.0.1");

    // UEFI should NOT have pxelinux.0/ldlinux.c32 in TFTP manifest
    let tftp_names: Vec<&str> = artifacts
        .tftp_manifest
        .iter()
        .map(|e| e.rel_path.as_str())
        .collect();
    assert!(tftp_names.contains(&"ipxe.efi"));
    assert!(
        !tftp_names.contains(&"pxelinux.0"),
        "UEFI should not have pxelinux.0"
    );
    assert!(
        !tftp_names.contains(&"ldlinux.c32"),
        "UEFI should not have ldlinux.c32"
    );

    // iPXE script should still reference correct ipxe binary in pxelinux.cfg
    let pxecfg = artifacts.find_file("pxelinux.cfg/default").unwrap();
    assert!(pxecfg.content.contains("KERNEL ipxe.efi"));
}

#[test]
fn pxe_uefi_arm64_artifacts() {
    let params = sample_pxe_params(BootMode::UefiArm64);
    let artifacts = PxeConfigBuilder::build(&params);

    assert_eq!(artifacts.dhcp.boot_filename, "ipxe-arm64.efi");

    let tftp_names: Vec<&str> = artifacts
        .tftp_manifest
        .iter()
        .map(|e| e.rel_path.as_str())
        .collect();
    assert!(tftp_names.contains(&"ipxe-arm64.efi"));

    let pxecfg = artifacts.find_file("pxelinux.cfg/default").unwrap();
    assert!(pxecfg.content.contains("KERNEL ipxe-arm64.efi"));
}

#[test]
fn pxe_ipxe_script_idempotent() {
    let params = sample_pxe_params(BootMode::Uefi);
    let s1 = PxeConfigBuilder::build_ipxe_script(&params);
    let s2 = PxeConfigBuilder::build_ipxe_script(&params);
    assert_eq!(s1, s2, "Same input must produce identical iPXE script");
}

#[test]
fn pxe_find_file_missing_returns_none() {
    let params = sample_pxe_params(BootMode::Uefi);
    let artifacts = PxeConfigBuilder::build(&params);
    assert!(artifacts.find_file("nonexistent.txt").is_none());
}

// ============================================================================
// 5. MockProvisioner PXE boot -> init_system 链
// ============================================================================

#[tokio::test]
async fn provisioner_pxe_boot_then_init_system_chain() {
    let provisioner = MockProvisioner::new().with_ready_node("os-node-01");
    let target = sample_target();
    let config = sample_config();

    // Phase 1a: PXE boot
    let boot_task = provisioner
        .boot_via_pxe(&target)
        .await
        .expect("PXE boot should succeed");
    let boot_status = provisioner.status(&boot_task).await;
    assert!(
        matches!(boot_status, ProvisionStatus::Booting),
        "After PXE boot, status should be Booting, got {:?}",
        boot_status
    );

    // Phase 1b: System init (partition -> pool -> base system)
    let init_task = provisioner
        .init_system(&target, &config)
        .await
        .expect("init_system should succeed");
    match provisioner.status(&init_task).await {
        ProvisionStatus::Ready { node_id } => {
            assert_eq!(node_id.as_str(), "os-node-01");
        }
        other => panic!("Expected Ready after init, got {:?}", other),
    }
}

#[tokio::test]
async fn provisioner_boot_failure_returns_error() {
    let provisioner = MockProvisioner::new().with_boot_failure(true);
    let target = sample_target();
    let err = provisioner
        .boot_via_pxe(&target)
        .await
        .expect_err("Boot should fail");
    assert!(
        matches!(err, os_provision::ProvisionError::PxeBootFailed(_)),
        "Expected PxeBootFailed, got {:?}",
        err
    );
}

#[tokio::test]
async fn provisioner_init_failure_returns_error() {
    let provisioner = MockProvisioner::new().with_init_failure(true);
    let target = sample_target();
    let config = sample_config();
    let err = provisioner
        .init_system(&target, &config)
        .await
        .expect_err("Init should fail");
    assert!(
        matches!(err, os_provision::ProvisionError::InitFailed(_)),
        "Expected InitFailed, got {:?}",
        err
    );
}

// ============================================================================
// 6. MockMigrationEngine plan/execute/resume + 排除键生成
// ============================================================================

#[tokio::test]
async fn migration_engine_plan_generates_exclude_keys_for_all_categories() {
    let engine = MockMigrationEngine::new();
    let source = NodeId::new("source-node");
    let target = NodeId::new("target-node");
    let datasets = vec![
        DatasetId::new("tank/media"),
        DatasetId::new("tank/photos"),
        DatasetId::new("tank/docs"),
    ];

    let plan = engine
        .plan(&source, &target, &datasets)
        .await
        .expect("plan should succeed");

    assert_eq!(plan.source_node, source);
    assert_eq!(plan.target_node, target);
    assert_eq!(plan.datasets.len(), 3);
    assert!(
        !plan.exclude_keys.is_empty(),
        "Plan should contain exclude keys from S3.19 categories"
    );
    assert_eq!(plan.resume_point, None);

    // Verify exclude_keys reference all S3.19 categories
    let all_cats = [
        "SystemCredential",
        "TlsPrivateKey",
        "SshPrivateKey",
        "SmbCredential",
        "DatabasePassword",
        "JwtTotpSecret",
        "WalletKey",
        "ClusterSecret",
    ];
    for cat in &all_cats {
        assert!(
            plan.exclude_keys.iter().any(|k| k.contains(cat)),
            "Exclude keys should reference category {}",
            cat
        );
    }
}

#[tokio::test]
async fn migration_engine_execute_and_status() {
    let engine = MockMigrationEngine::new();
    let plan = MigrationPlan {
        source_node: NodeId::new("src"),
        target_node: NodeId::new("dst"),
        datasets: vec![DatasetId::new("tank/data")],
        exclude_keys: vec!["exclude::SystemCredential".into()],
        resume_point: None,
    };

    let task_id = engine.execute(plan).await.expect("execute should succeed");
    let status = engine.status(&task_id).await;
    assert!(
        matches!(status, MigrationStatus::Completed),
        "After execute, status should be Completed, got {:?}",
        status
    );
}

#[tokio::test]
async fn migration_engine_resume_completes() {
    let engine = MockMigrationEngine::new();
    let task_id = engine
        .resume("any-plan-id")
        .await
        .expect("resume should succeed");
    assert!(
        matches!(engine.status(&task_id).await, MigrationStatus::Completed),
        "Resume should produce a Completed task"
    );
}

#[tokio::test]
async fn migration_engine_plan_failure() {
    let engine = MockMigrationEngine::new().with_plan_failure(true);
    let err = engine
        .plan(&NodeId::new("a"), &NodeId::new("b"), &[])
        .await
        .expect_err("Plan should fail");
    assert!(
        matches!(err, os_provision::ProvisionError::MigrationFailed(_)),
        "Expected MigrationFailed, got {:?}",
        err
    );
}

#[tokio::test]
async fn migration_engine_execute_failure() {
    let engine = MockMigrationEngine::new().with_execute_failure(true);
    let plan = MigrationPlan {
        source_node: NodeId::new("a"),
        target_node: NodeId::new("b"),
        datasets: vec![],
        exclude_keys: vec![],
        resume_point: None,
    };
    let err = engine.execute(plan).await.expect_err("Execute should fail");
    assert!(
        matches!(err, os_provision::ProvisionError::MigrationFailed(_)),
        "Expected MigrationFailed, got {:?}",
        err
    );
}

// ============================================================================
// 7. 端到端集成：Phase + Exclude + Checkpoint + PXE + Mock 全链路
// ============================================================================

#[tokio::test]
async fn full_provision_bootstrap_chain() {
    // This test orchestrates the full PXE bootstrap chain:
    // 1. Build PXE config artifacts
    // 2. MockProvisioner boots via PXE + init_system
    // 3. MockMigrationEngine plans migration (generates exclude keys)
    // 4. PhaseMachine advances through all 4 phases
    // 5. ExcludeRules filters sensitive entries in migration path
    // 6. Checkpoint tracks progress and can resume

    let pxe_params = sample_pxe_params(BootMode::Uefi);
    let artifacts = PxeConfigBuilder::build(&pxe_params);

    // Verify PXE artifacts are valid before proceeding
    assert!(artifacts.find_file("bootstrap.ipxe").is_some());
    assert_eq!(artifacts.dhcp.boot_filename, "ipxe.efi");

    // Phase 1: PXE boot + system init
    let provisioner = MockProvisioner::new().with_ready_node("target-os");
    let target = sample_target();
    let config = sample_config();

    let boot_task = provisioner.boot_via_pxe(&target).await.unwrap();
    assert!(matches!(
        provisioner.status(&boot_task).await,
        ProvisionStatus::Booting
    ));

    let init_task = provisioner.init_system(&target, &config).await.unwrap();
    match provisioner.status(&init_task).await {
        ProvisionStatus::Ready { node_id } => assert_eq!(node_id.as_str(), "target-os"),
        other => panic!("Expected Ready, got {:?}", other),
    }

    // Phase 2: Plan migration with exclude rules
    let engine = MockMigrationEngine::new();
    let source = NodeId::new("source-os");
    let target_node = NodeId::new("target-os");
    let datasets = vec![DatasetId::new("tank/media"), DatasetId::new("tank/shared")];

    let plan = engine.plan(&source, &target_node, &datasets).await.unwrap();
    assert!(!plan.exclude_keys.is_empty());

    // Execute migration
    let migrate_task = engine.execute(plan).await.unwrap();
    assert!(matches!(
        engine.status(&migrate_task).await,
        MigrationStatus::Completed
    ));

    // Verify exclude rules in migration path
    let rules = ExcludeRules::defaults();
    let migration_entries = [
        "/etc/os/config.toml",
        "/etc/shadow",
        "/etc/ssh/ssh_host_rsa_key",
        "/etc/os/jwt-signing.key",
        "/etc/samba/smb.conf",
    ];
    let (transfer, excluded) = rules.partition(migration_entries.iter().copied());
    assert!(transfer.contains(&"/etc/os/config.toml"));
    assert!(transfer.contains(&"/etc/samba/smb.conf"));
    assert_eq!(excluded.len(), 3);

    // Phase machine full lifecycle
    let mut pm = PhaseMachine::new();
    for expected_phase in MigrationPhase::all() {
        assert_eq!(pm.current(), *expected_phase);
        if !pm.is_finished() {
            assert!(matches!(pm.advance(), PhaseTransition::Advance { .. }));
        }
    }
    assert!(pm.is_finished());

    // Checkpoint progression
    let mut cp = MigrationCheckpoint::initial("full-chain-plan");
    let all_datasets = vec![DatasetId::new("tank/media"), DatasetId::new("tank/shared")];
    let pending_excludes = vec!["/etc/shadow".to_string()];

    // Initially: resume from SystemInit
    let policy = CheckpointPolicy;
    match policy.decide_resume(
        Some(&cp),
        MigrationPhase::all(),
        &all_datasets,
        &pending_excludes,
        &rules,
    ) {
        ResumeDecision::ResumeFromPhase { phase, .. } => {
            assert_eq!(phase, MigrationPhase::SystemInit);
        }
        _ => panic!("Should resume from SystemInit"),
    }

    // After completing all phases: Finished
    cp.completed_phases = MigrationPhase::all().to_vec();
    cp.current_phase = MigrationPhase::FirstBoot;
    assert_eq!(
        policy.decide_resume(
            Some(&cp),
            MigrationPhase::all(),
            &all_datasets,
            &pending_excludes,
            &rules,
        ),
        ResumeDecision::Finished
    );
}

// ============================================================================
// 8. BootMode 属性验证
// ============================================================================

#[test]
fn boot_mode_default_bootfiles() {
    assert_eq!(BootMode::Bios.default_bootfile(), "pxelinux.0");
    assert_eq!(BootMode::Bios.default_ipxe_binary(), "undionly.kpxe");
    assert_eq!(BootMode::Uefi.default_bootfile(), "ipxe.efi");
    assert_eq!(BootMode::Uefi.default_ipxe_binary(), "ipxe.efi");
    assert_eq!(BootMode::UefiArm64.default_bootfile(), "ipxe-arm64.efi");
    assert_eq!(BootMode::UefiArm64.default_ipxe_binary(), "ipxe-arm64.efi");
}
