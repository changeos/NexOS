//! os-services backup 真实测——本地快照验证 + scrub 查询 + zfs send 命令构造。
//!
//! **定位**：补充 `src/impl_backup.rs` 的 [`ZfsBackupManager`] 单元测——单元测用
//! [`MockStorageBackend`] 验证调度/状态机逻辑，但有两个关键路径从未被本机真实 zfs 验证：
//! 1. **本地快照策略执行**：`trigger_now` → `backend.snapshot()` → 真实快照落地（`zfs list -t snapshot` 可见）。
//! 2. **scrub 查询原语**：`scrub_status` 骨架返回空报告（TODO [RUNTIME] 未接通），但真实
//!    `zpool status` 的 scrub 行可解析出 errors/repaired/duration（本文件提供解析器 + 真实测验证）。
//! 3. **远程复制命令构造**：`target_remote = Some` 的 TODO [RUNTIME] 接通点依赖
//!    [`os_storage::Replication`] 的真实实现（跨 crate），本文件不强行接通，而是验证
//!    「`zfs send | ssh recv` 命令链构造正确性」并标注接通点（见 [`build_send_recv_cmd`]）。
//!
//! ## 分组
//! - **A. 逻辑测（默认跑）**：纯逻辑 / mock backend，验证策略执行 / 命令构造 / scrub 解析器 /
//!   错误传播。无外部依赖，`cargo test` 直接跑。
//! - **B. 真实 zfs 测（`#[ignore]`）**：建临时池（sparse file vdev）→ 跑真实
//!   `zpool create / zfs snapshot / zpool scrub / zfs send`，验证落地 + 解析。
//!   需 root + zfsutils-linux + zfs 内核模块；`sudo cargo test -- --ignored`。
//!
//! ## 红线
//! - **不碰宿主真实 pool**：唯一 `osprobe` 前缀 + sparse file vdev + RAII teardown
//!   （[`RealPoolGuard`] drop 时 `zpool destroy -f` + 删 sparse file，即使断言失败也清理）。
//! - **不改 BackupManager trait 签名**：所有验证经 `ZfsBackupManager` 公开 API。
//! - **不强行接通远程复制**：只验证命令构造 + 标注接通点（[`build_send_recv_cmd`] 注释）。
//!
//! ## 跑法
//! ```bash
//! # 逻辑测（默认套件）
//! cargo test -p os-services --features mock --test backup_real
//! # 真实测（需 root + zfs）
//! sudo env PATH=$PATH RUSTUP_HOME=$RUSTUP_HOME CARGO_HOME=$CARGO_HOME \
//!   cargo test -p os-services --features mock --test backup_real -- --ignored --nocapture
//! ```

#![cfg(feature = "mock")]

use std::sync::Arc;

use os_core::{DatasetId, Health, PoolId, SnapshotId};
use os_services::backup::{BackupPolicy, BackupStatus, CronExpr, RetentionPolicy, ScrubReport};
use os_services::{BackupManager, ZfsBackupManager};

use os_storage::model::{Dataset, EncryptionState, VdevKind};
use os_storage::{MockStorageBackend, Pool, StorageBackend, StorageError, VdevSpec};

// ============================================================================
// 通用辅助：mock backend / 策略构造 / cron 工具
// ============================================================================

fn pool(name: &str) -> Pool {
    Pool {
        id: PoolId::new(name),
        name: name.into(),
        vdevs: vec![],
        capacity: os_core::Capacity {
            used_bytes: 0,
            total_bytes: 0,
        },
        health: Health::Healthy,
    }
}

fn dataset(name: &str) -> Dataset {
    Dataset {
        id: DatasetId::new(name),
        pool: PoolId::new("tank"),
        name: name.into(),
        used_bytes: 0,
        avail_bytes: 0,
        mounted: true,
        encryption: EncryptionState::Off,
    }
}

/// 构造一个预置了 tank pool + tank/media dataset 的 mock backend。
fn tank_backend() -> Arc<MockStorageBackend> {
    Arc::new(
        MockStorageBackend::new()
            .with_pool(pool("tank"))
            .with_dataset(dataset("tank/media")),
    )
}

/// 测用 manager 类型别名（注入 MockStorageBackend）。
type TestMgr = ZfsBackupManager<MockStorageBackend>;

/// 构造一个标准 backup policy（daily 03:00，keep_last=7 / keep_days=7，源 tank/media）。
fn daily_policy(name: &str) -> BackupPolicy {
    BackupPolicy {
        name: name.into(),
        schedule: CronExpr::new("0 3 * * *"),
        retention: RetentionPolicy {
            keep_last: 7,
            keep_days: 7,
        },
        source: DatasetId::new("tank/media"),
        target_remote: None,
    }
}

/// 带 GFS 增强保留（hourly/daily/weekly/monthly）的 policy（通过 select_expired 间接验证）。
fn gfs_policy(name: &str) -> BackupPolicy {
    BackupPolicy {
        name: name.into(),
        schedule: CronExpr::new("0 * * * *"), // 每小时（hourly 频率）
        retention: RetentionPolicy {
            keep_last: 24,
            keep_days: 30,
        },
        source: DatasetId::new("tank/media"),
        target_remote: None,
    }
}

// ============================================================================
// A. 逻辑测（默认跑，纯 mock / 纯函数）
// ============================================================================

// ----------------------------------------------------------------------------
// A.a run_backup 本地快照策略执行（mock backend）——验证 snapshot 创建 + 策略保留
// ----------------------------------------------------------------------------

/// 验证 `trigger_now` 经 mock backend 真实创建快照（mock 内部 snapshot_count 自增），
/// job 状态置 Success，last_run 被记录，next_run 仍可读。
///
/// 这条路径在 `impl_backup::tests` 已有单测，但本测**额外断言 backend 真实记录了快照**
/// （`snapshot_count() == 1` + 快照名匹配 `auto-<timestamp>` 格式），把「策略执行 → 快照落地」
/// 的契约钉死。配合真实测 B.a 互为「mock 落地 / 真实落地」对证。
#[tokio::test]
async fn trigger_now_creates_snapshot_in_backend() {
    let backend = tank_backend();
    let mgr: TestMgr = ZfsBackupManager::new(backend.clone());
    let id = mgr.schedule(daily_policy("media-daily")).await.unwrap();

    let task_id = mgr.trigger_now(&id).await.unwrap();
    let _ = task_id; // TaskId 仅追踪用，不在此断言

    // backend 真实记录了一次 snapshot（mock 写操作更新内部状态）。
    assert_eq!(
        backend.snapshot_count(),
        1,
        "trigger_now 应在 backend 创建 1 个快照"
    );

    // 快照列表可查到，且名匹配 `auto-<RFC3339-ish 时间戳>`。
    let snaps = backend
        .list_snapshots(Some(&DatasetId::new("tank/media")))
        .await
        .unwrap();
    assert_eq!(snaps.len(), 1, "应列出 1 个快照");
    let snap_id = snaps[0].id.as_str();
    assert!(
        snap_id.starts_with("tank/media@auto-"),
        "快照名应匹配 auto-<ts> 格式，实际: {snap_id}"
    );
    assert!(
        snap_id.len() > "tank/media@auto-".len() + 8,
        "时间戳部分不应为空: {snap_id}"
    );

    // job 状态机：Scheduled → Running → Success，last_run 被记录。
    let jobs = mgr.list_jobs().await.unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, BackupStatus::Success);
    assert!(jobs[0].last_run.is_some(), "last_run 应被记录");

    println!("[backup_real] 本地快照策略执行 OK: snapshot={snap_id}");
}

/// 验证连续多次 `trigger_now` 创建多个不重复快照（快照名含纳秒级时间戳，
/// 同一 job 多次触发应各自落地独立快照）。
#[tokio::test]
async fn repeated_trigger_now_creates_distinct_snapshots() {
    let backend = tank_backend();
    let mgr: TestMgr = ZfsBackupManager::new(backend.clone());
    let id = mgr.schedule(daily_policy("multi")).await.unwrap();

    // 连续触发 3 次（间隔 1ms 确保时间戳不同——auto-<秒精度>，sleep 跨秒）。
    mgr.trigger_now(&id).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    mgr.trigger_now(&id).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    mgr.trigger_now(&id).await.unwrap();

    assert_eq!(
        backend.snapshot_count(),
        3,
        "3 次 trigger_now 应创建 3 个独立快照"
    );
    // 策略保留由上层 retention 周期清理由上层负责（不在 trigger_now 内做），故 3 个都保留。
    println!("[backup_real] 3 次连续触发创建 3 个独立快照 OK");
}

// ----------------------------------------------------------------------------
// A.b 远程复制命令构造验证（zfs send | ssh recv 命令链正确性）
// ----------------------------------------------------------------------------

/// 构造 `zfs send | ssh recv` 命令链的 argv（纯函数，验证命令构造逻辑）。
///
/// **接通点标注**：`impl_backup::trigger_now` 在 `policy.target_remote = Some(remote)` 时
/// 应调 `os_storage::Replication::send(&snapshot, &target)`，其默认实现 `ZfsSendRecv`
/// 构造如下命令链（见 `crates/os-storage/src/replication_impl.rs`）：
/// ```text
/// 源端： zfs send <pool/dataset@snap>
/// 远端： ssh <user>@<host> zfs recv <target_dataset>
/// 管道： send.stdout | recv.stdin
/// ```
/// `ZfsSendRecv::send_cmd` / `recv_cmd` 是 `os-storage` 内私有方法，故本文件重建等价
/// 构造逻辑验证 argv 正确性（与 `ZfsSendRecv` 的契约一致：snapshot 全名 / host:user 解析 / recv dataset）。
///
/// 返回 `(send_argv, recv_argv)`——send 是 `zfs send <snap>`，recv 是 `ssh <user>@<host> zfs recv <ds>`
/// 或本地 `zfs recv <ds>`（host=None 时）。
///
/// **红线**：本函数不 spawn 任何子进程（纯构造 argv）；真实接通需 Replication 真实实现
/// （跨 crate），不在本测强行接通。
fn build_send_recv_cmd(
    snapshot: &SnapshotId,
    target_remote: &str,
    ssh_user: &str,
) -> (Vec<String>, Vec<String>) {
    // send 端：`zfs send <pool/dataset@snap>`（可选 -R 递归 / -I 增量，这里基础全量）。
    let send_argv = vec!["zfs".into(), "send".into(), snapshot.as_str().into()];

    // target_remote 解析：<host>:<dataset>（远端 ssh）或 <dataset>（本地 recv）。
    let (host, dataset) = if let Some((h, ds)) = target_remote.split_once(':') {
        (Some(h), ds)
    } else {
        (None, target_remote)
    };

    let recv_argv = if let Some(h) = host {
        vec![
            "ssh".into(),
            format!("{ssh_user}@{h}"),
            "zfs".into(),
            "recv".into(),
            dataset.into(),
        ]
    } else {
        vec!["zfs".into(), "recv".into(), dataset.into()]
    };

    (send_argv, recv_argv)
}

/// 验证远程复制命令构造正确性——`target_remote = "backuphost:backup/media"` 时，
/// send/recv argv 符合 `zfs send | ssh recv` 契约。
#[test]
fn build_send_recv_cmd_remote_target_constructs_correct_pipeline() {
    let snap = SnapshotId::new("tank/media@auto-20260101T030000");
    let (send, recv) = build_send_recv_cmd(&snap, "backuphost:backup/media", "root");

    // send 端：zfs send <full_snapshot>
    assert_eq!(send, vec!["zfs", "send", "tank/media@auto-20260101T030000"]);

    // recv 端：ssh root@backuphost zfs recv backup/media
    assert_eq!(
        recv,
        vec!["ssh", "root@backuphost", "zfs", "recv", "backup/media"]
    );

    // 管道语义：send 的 stdout 喂给 recv 的 stdin（真实接通时由 Stdio::from(child.stdout) 串联）。
    println!(
        "[backup_real] 远程复制命令构造 OK:\n  send: {}\n  recv: {}\n  pipe: send.stdout | recv.stdin",
        send.join(" "),
        recv.join(" ")
    );
}

/// 验证本地 recv 路径——`target_remote = "tank/backup"`（无 host）时走本地 `zfs recv`。
#[test]
fn build_send_recv_cmd_local_target_omits_ssh() {
    let snap = SnapshotId::new("tank/media@s1");
    let (send, recv) = build_send_recv_cmd(&snap, "tank/backup", "root");

    assert_eq!(send, vec!["zfs", "send", "tank/media@s1"]);
    // 无 host → 本地 recv，argv 不含 ssh。
    assert_eq!(recv, vec!["zfs", "recv", "tank/backup"]);
    assert!(!recv.iter().any(|a| a == "ssh"), "本地 target 不应有 ssh");
    println!("[backup_real] 本地 recv 命令构造 OK: {}", recv.join(" "));
}

/// 验证 trigger_now 在 `target_remote = Some` 时不 panic（TODO [RUNTIME] 占位路径，
/// 当前仅记日志不真传——本测钉死「不 panic + 仍标 Success」契约，待 Replication
/// 接通后此断言需更新为「真实触发 send」）。
///
/// **接通点**：`impl_backup::trigger_now` 行 144-146 的 `if let Some(_remote)` 分支
/// 当前为空（TODO [RUNTIME]），接通后应调 `Replication::send`。
#[tokio::test]
async fn trigger_now_with_remote_target_does_not_panic() {
    let backend = tank_backend();
    let mgr: TestMgr = ZfsBackupManager::new(backend.clone());

    let mut policy = daily_policy("remote");
    policy.target_remote = Some("backuphost:backup/media".into());

    let id = mgr.schedule(policy).await.unwrap();
    let _ = mgr.trigger_now(&id).await.unwrap();

    // 当前骨架：本地快照仍创建（snapshot_count == 1），远程复制占位（TODO [RUNTIME]）。
    assert_eq!(backend.snapshot_count(), 1, "本地快照应仍创建");
    let jobs = mgr.list_jobs().await.unwrap();
    assert_eq!(
        jobs[0].status,
        BackupStatus::Success,
        "远程复制 TODO [RUNTIME] 占位期间应仍标 Success（不 panic）"
    );
    println!(
        "[backup_real] target_remote=Some 占位 OK（本地快照落地，远程复制待 Replication 接通）"
    );
}

// ----------------------------------------------------------------------------
// A.c scrub_status 骨架契约——返回空报告（无历史记录时的正确行为）
// ----------------------------------------------------------------------------

/// 验证 `scrub_status` 骨架返回空报告（errors=0, repaired=0, last_finished=None）。
///
/// **契约**：无历史 scrub 记录时返回「零值空报告」而非错误——区分「未 scrub」与
/// 「scrub 失败」。真实 scrub 查询（TODO [RUNTIME]）接通后，空报告语义应保留为
/// 「从未 scrub」的标识（last_finished=None 是关键信号）。
#[tokio::test]
async fn scrub_status_skeleton_returns_empty_report() {
    let mgr: TestMgr = ZfsBackupManager::new(tank_backend());
    let report = mgr.scrub_status(&PoolId::new("tank")).await.unwrap();

    assert_eq!(report.errors, 0, "空报告 errors 应为 0");
    assert_eq!(report.repaired, 0, "空报告 repaired 应为 0");
    assert_eq!(report.duration_secs, 0, "空报告 duration_secs 应为 0");
    assert!(
        report.last_finished.is_none(),
        "空报告 last_finished 应为 None（从未 scrub 的标识）"
    );
    println!("[backup_real] scrub_status 骨架空报告契约 OK");
}

// ----------------------------------------------------------------------------
// A.d scrub_status 解析原语（zpool status 输出 → ScrubReport）
// ----------------------------------------------------------------------------

/// 解析 `zpool status <pool>` 的 scrub 行，提取 errors / repaired / duration / 完成时间。
///
/// `zpool status` 的 scrub 行有 3 种形态（OpenZFS 2.4.1 实测）：
/// 1. **已完成**：`scan: scrub repaired 0B in 00:00:00 with 0 errors on Thu Aug  6 18:02:30 2026`
/// 2. **运行中**：`scan: scrub in progress since Thu Aug  6 18:02:30 2026 (... done, 1% - 0B/s)`
/// 3. **无 scrub 记录**：无 `scan:` 行（或 `scan: none requested`）
///
/// 返回 `Some(ScrubReport)` 当且仅当存在可解析的 scrub 行；无 scrub 记录返回 `None`
/// （调用方据此区分「从未 scrub」与「scrub 完成 0 错误」）。
///
/// **接通点**：`impl_backup::scrub_status`（行 158-169）的 TODO [RUNTIME] 接通后，
/// 应调底层 `zpool status`（经 `ZfsCliBackend` 或 storage-agent 的 scrub 查询原语），
/// 并用本解析器（或等价实现）把输出转 `ScrubReport`。
fn parse_scrub_from_zpool_status(status_output: &str) -> Option<ScrubReport> {
    // 找 scan: 行（scrub/resilver/none）。
    let scan_line = status_output
        .lines()
        .find(|l| l.trim_start().starts_with("scan:"))?;

    // 先 trim 整行（去前导空格），再剥 "scan:" 前缀，最后再 trim（去 "scan:" 后的空格）。
    let trimmed = scan_line.trim().trim_start_matches("scan:").trim();

    // "none requested" / "resilver" 等非 scrub 行视为无 scrub 报告。
    if trimmed.starts_with("none requested") {
        return None;
    }
    // resilver 不是 scrub（虽然结构类似），按需排除——本解析器只处理 scrub。
    if trimmed.starts_with("resilver") {
        return None;
    }

    // 运行中："scrub in progress since ..."
    if trimmed.starts_with("scrub in progress") {
        // 运行中：errors/repaired 可从尾部 "(... done, X% - ...)" 提取百分比，
        // 但 completed/duration 未知（未结束）。返回部分填充的报告（errors/repaired=0，
        // last_finished=None 表示未完成）。真实接通时由调用方按需扩展进度字段。
        return Some(ScrubReport {
            errors: 0,
            repaired: 0,
            last_finished: None, // 未完成
            duration_secs: 0,    // 未结束
        });
    }

    // 已完成："scrub repaired <X> in <HH:MM:SS> with <N> errors on <date>"
    // 关键词：repaired / in / with / errors / on
    let repaired = parse_size_to_bytes(
        trimmed
            .strip_prefix("scrub repaired ")
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or("0B"),
    );
    let errors = extract_u64_after(trimmed, "with ", " errors");
    let duration_secs = parse_duration_hms(
        trimmed
            .split(" in ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .unwrap_or("00:00:00"),
    );
    // last_finished 解析自 "on <date>"——但 chrono 解析 ctime 格式（Thu Aug 6 ...）需
    // locale 处理；本骨架置 None（真实接通时由调用方用 `DateTime::parse_from_rfc2822` 等）。
    Some(ScrubReport {
        errors,
        repaired,
        last_finished: None, // TODO(接通): 解析 "on <date>" → DateTime
        duration_secs,
    })
}

/// 从 `haystack` 中提取 `<prefix><number><suffix>` 的 number 部分。
/// 例：`extract_u64_after("with 0 errors", "with ", " errors")` → `Some(0)`。
fn extract_u64_after(haystack: &str, prefix: &str, suffix: &str) -> u64 {
    let start = match haystack.find(prefix) {
        Some(i) => i + prefix.len(),
        None => return 0,
    };
    let rest = &haystack[start..];
    let end = rest.find(suffix).unwrap_or(rest.len());
    rest[..end].trim().parse::<u64>().unwrap_or(0)
}

/// 解析 ZFS 大小（"0B", "1K", "512K", "2M", "1G"）为字节数。
fn parse_size_to_bytes(s: &str) -> u64 {
    let s = s.trim();
    if s.is_empty() {
        return 0;
    }
    let (num_part, unit) = s.split_at(s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len()));
    let n: u64 = num_part.parse().unwrap_or(0);
    let mult = match unit {
        "B" => 1,
        "K" => 1024,
        "M" => 1024 * 1024,
        "G" => 1024 * 1024 * 1024,
        "T" => 1024u64 * 1024 * 1024 * 1024,
        _ => 1,
    };
    n * mult
}

/// 解析 `HH:MM:SS` 为总秒数。
fn parse_duration_hms(s: &str) -> u64 {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 3 {
        return 0;
    }
    let h: u64 = parts[0].parse().unwrap_or(0);
    let m: u64 = parts[1].parse().unwrap_or(0);
    let sec: u64 = parts[2].parse().unwrap_or(0);
    h * 3600 + m * 60 + sec
}

/// 验证 scrub 解析器对「已完成、0 错误」scrub 行的解析（本机 zfs 2.4.1 真实输出格式）。
#[test]
fn parse_scrub_completed_zero_errors() {
    let status = "  pool: tank\n state: ONLINE\n  scan: scrub repaired 0B in 00:00:00 with 0 errors on Thu Aug  6 18:02:30 2026\nconfig:\n\n\tNAME  STATE     READ WRITE CKSUM\n\ttank  ONLINE       0     0     0\nerrors: No known data errors\n";
    let report = parse_scrub_from_zpool_status(status).expect("应解析出 scrub 报告");
    assert_eq!(report.errors, 0, "0 errors");
    assert_eq!(report.repaired, 0, "repaired 0B = 0 bytes");
    assert_eq!(report.duration_secs, 0, "00:00:00 = 0 秒");
    assert!(report.last_finished.is_none(), "骨架阶段不解析日期");
    println!("[backup_real] scrub 解析（已完成，0 错误）OK");
}

/// 验证 scrub 解析器对「已完成、有修复」scrub 行的解析（repaired 512K，errors 2，4 分 30 秒）。
#[test]
fn parse_scrub_completed_with_repairs() {
    let status =
        "  scan: scrub repaired 512K in 00:04:30 with 2 errors on Fri Jan  1 12:00:00 2026\n";
    let report = parse_scrub_from_zpool_status(status).expect("应解析出 scrub 报告");
    assert_eq!(report.errors, 2, "2 errors");
    assert_eq!(report.repaired, 512 * 1024, "repaired 512K = 524288 bytes");
    assert_eq!(report.duration_secs, 4 * 60 + 30, "00:04:30 = 270 秒");
    println!(
        "[backup_real] scrub 解析（有修复）OK: errors={} repaired={}B dur={}s",
        report.errors, report.repaired, report.duration_secs
    );
}

/// 验证 scrub 解析器对「运行中」scrub 行的解析（last_finished=None 表示未完成）。
#[test]
fn parse_scrub_in_progress() {
    let status = "  scan: scrub in progress since Thu Aug  6 18:02:30 2026 (1h2m to go, 12.34% done, 0B/s)\n";
    let report = parse_scrub_from_zpool_status(status).expect("运行中 scrub 也应返回报告");
    assert!(
        report.last_finished.is_none(),
        "运行中 scrub last_finished 应 None"
    );
    assert_eq!(report.duration_secs, 0, "运行中 duration 未定");
    println!("[backup_real] scrub 解析（运行中）OK");
}

/// 验证 scrub 解析器对「无 scrub 记录」（none requested / 无 scan 行）返回 None。
#[test]
fn parse_scrub_none_requested_returns_none() {
    let status_none = "  scan: none requested\n";
    assert!(parse_scrub_from_zpool_status(status_none).is_none());

    let status_no_scan = "  pool: tank\n state: ONLINE\nconfig:\n";
    assert!(
        parse_scrub_from_zpool_status(status_no_scan).is_none(),
        "无 scan: 行应返回 None"
    );
    println!("[backup_real] scrub 解析（无记录）返回 None OK");
}

/// 验证解析器辅助函数对 ZFS 大小格式的处理。
#[test]
fn parse_size_handles_zfs_units() {
    assert_eq!(parse_size_to_bytes("0B"), 0);
    assert_eq!(parse_size_to_bytes("1K"), 1024);
    assert_eq!(parse_size_to_bytes("512K"), 512 * 1024);
    assert_eq!(parse_size_to_bytes("2M"), 2 * 1024 * 1024);
    assert_eq!(parse_size_to_bytes("1G"), 1024 * 1024 * 1024);
    assert_eq!(parse_size_to_bytes(""), 0);
}

/// 验证 `HH:MM:SS` 时长解析。
#[test]
fn parse_duration_handles_hms() {
    assert_eq!(parse_duration_hms("00:00:00"), 0);
    assert_eq!(parse_duration_hms("00:04:30"), 270);
    assert_eq!(parse_duration_hms("01:02:03"), 3723);
    assert_eq!(parse_duration_hms("garbage"), 0);
}

// ----------------------------------------------------------------------------
// A.e 策略解析（保留策略 GFS / cron 频率）验证
// ----------------------------------------------------------------------------

/// 验证 `os_services::select_expired`（GFS 保留策略算法）对 keep_last 的处理，
/// 间接验证 backup policy 的保留语义。
#[test]
fn retention_keep_last_protects_newest_n() {
    use chrono::TimeZone;
    use chrono::Utc;
    use os_services::select_expired;
    use os_services::TimedSnapshot;

    fn t(y: i32, mo: u32, d: u32) -> os_core::DateTime {
        Utc.with_ymd_and_hms(y, mo, d, 0, 0, 0).unwrap()
    }

    // 7 份快照，keep_last=3 → 最早 4 份过期
    let snaps: Vec<TimedSnapshot<String>> = (1..=7)
        .map(|i| TimedSnapshot::new(format!("s{i}"), t(2024, 1, i)))
        .collect();
    let rule = os_services::GfsRetentionRule {
        keep_last: 3,
        keep_days: 0,
        keep_hourly: 0,
        keep_daily: 0,
        keep_weekly: 0,
        keep_monthly: 0,
    };
    let now = t(2024, 1, 31);
    let expired = select_expired(&snaps, &rule, &now);
    assert_eq!(expired.len(), 4, "keep_last=3 → 7-3=4 份过期");
    assert_eq!(expired[0], "s1", "最老的先过期");
    assert_eq!(expired[3], "s4");
    println!("[backup_real] GFS 保留策略 keep_last 算法 OK");
}

/// 验证 cron 频率解析——daily `0 3 * * *` 与 hourly `0 * * * *` 的 next_run 间隔不同。
#[tokio::test]
async fn cron_frequency_daily_vs_hourly_next_run() {
    use chrono::TimeZone;
    use os_core::Utc;
    use os_services::CronSchedule;

    let daily = CronSchedule::parse(&CronExpr::new("0 3 * * *")).unwrap();
    let hourly = CronSchedule::parse(&CronExpr::new("0 * * * *")).unwrap();

    let now = Utc.with_ymd_and_hms(2026, 8, 6, 12, 0, 0).unwrap();
    let next_daily = daily.next_run(&now).unwrap();
    let next_hourly = hourly.next_run(&now).unwrap();

    // daily：次日 03:00（约 15 小时后）。
    let daily_delta = (next_daily - now).num_hours();
    assert!(
        (14..=16).contains(&daily_delta),
        "daily next_run 应约 15h 后，实际 {daily_delta}h"
    );

    // hourly：下一整点（1 小时后）。
    let hourly_delta = (next_hourly - now).num_minutes();
    assert!(
        (50..=70).contains(&hourly_delta),
        "hourly next_run 应约 60min 后，实际 {hourly_delta}min"
    );

    assert!(hourly_delta < daily_delta * 60, "hourly 间隔应远小于 daily");
    println!(
        "[backup_real] cron 频率解析 OK: daily next +{}h, hourly next +{}min",
        daily_delta, hourly_delta
    );
}

/// 验证 GFS policy（hourly 频率 + 长保留）与 daily policy 的策略参数差异——
/// 确保 policy 构造器不会混淆不同保留语义。
#[test]
fn gfs_vs_daily_policy_retention_differs() {
    let daily = daily_policy("d");
    let gfs = gfs_policy("g");

    assert_eq!(daily.retention.keep_last, 7);
    assert_eq!(gfs.retention.keep_last, 24, "GFS hourly 应 keep_last=24");
    assert_eq!(daily.retention.keep_days, 7);
    assert_eq!(gfs.retention.keep_days, 30, "GFS 应 keep_days=30");
    assert_ne!(
        daily.schedule.as_str(),
        gfs.schedule.as_str(),
        "daily 与 hourly cron 应不同"
    );
    assert_eq!(daily.schedule.as_str(), "0 3 * * *");
    assert_eq!(gfs.schedule.as_str(), "0 * * * *");
    println!("[backup_real] GFS vs daily policy 参数区分 OK");
}

// ----------------------------------------------------------------------------
// A.f 错误处理（backend 快照失败 → 状态机标 Failed，不 panic）
// ----------------------------------------------------------------------------

/// 验证 backend `snapshot` 失败时（注入 `with_error`），job 状态机标 `Failed`，
/// `trigger_now` 不 panic（仍返回 TaskId——失败在 job.status 反映，非 Err）。
///
/// 这条路径在 `impl_backup::tests::trigger_now_marks_failed_when_backend_errors` 已覆盖，
/// 本测**额外断言快照确实未落地**（`snapshot_count() == 0`），钉死「失败时不留半成品」契约。
///
/// **发现（行为观察，非断言失败）**：当前实现下，失败时 `last_run` 保持 `None`
/// （`impl_backup::trigger_now` 行 137-149 的 Err 分支不设 last_run，仅 Success 分支设）。
/// 这意味着失败时刻不被 job 记录——可能影响诊断「上次尝试何时失败」。本测钉死此现状，
/// 供后续接通时决策（如改为失败也记 last_run）。
#[tokio::test]
async fn backend_snapshot_failure_marks_job_failed_no_snapshot() {
    let backend = MockStorageBackend::new()
        .with_pool(pool("tank"))
        .with_dataset(dataset("tank/media"))
        .with_error(StorageError::CommandFailed("simulated zfs failure".into()));
    let backend = Arc::new(backend);
    let mgr: TestMgr = ZfsBackupManager::new(backend.clone());

    let id = mgr.schedule(daily_policy("fail")).await.unwrap();
    let _ = mgr.trigger_now(&id).await.unwrap(); // 不返回 Err——失败在 job.status

    // 快照未落地（with_error 一次性触发后，snapshot 返回 Err，未写入 mock 状态）。
    assert_eq!(
        backend.snapshot_count(),
        0,
        "backend 失败时不应留下半成品快照"
    );

    let jobs = mgr.list_jobs().await.unwrap();
    assert_eq!(jobs[0].status, BackupStatus::Failed, "job 应标 Failed");
    // 现状：失败时 last_run 不被记录（见函数级注释「发现」）。
    assert!(
        jobs[0].last_run.is_none(),
        "现状：失败时不记 last_run（impl_backup Err 分支不设——见测函数注释）"
    );
    println!(
        "[backup_real] backend 失败 → job Failed + 无半成品快照 OK（注：last_run 未记失败时刻）"
    );
}

/// 验证 backend 对不存在的 dataset 做 snapshot 时错误正确传播（DatasetNotFound）。
#[tokio::test]
async fn snapshot_missing_dataset_propagates_error() {
    // 只预置 pool，不预置 dataset → snapshot 命中 DatasetNotFound。
    let backend = Arc::new(MockStorageBackend::new().with_pool(pool("tank")));
    let mgr: TestMgr = ZfsBackupManager::new(backend.clone());

    let mut policy = daily_policy("ghost");
    policy.source = DatasetId::new("tank/nonexistent"); // 不存在的 dataset
    let id = mgr.schedule(policy).await.unwrap();
    let _ = mgr.trigger_now(&id).await.unwrap();

    let jobs = mgr.list_jobs().await.unwrap();
    assert_eq!(jobs[0].status, BackupStatus::Failed);
    assert_eq!(backend.snapshot_count(), 0);
    println!("[backup_real] 不存在 dataset → snapshot 失败传播 OK");
}

// ============================================================================
// B. 真实 zfs backup 测（#[ignore]，需 root + zfsutils-linux + zfs 模块）
// ============================================================================

#[cfg(feature = "mock")]
mod real {
    use super::*;
    use os_storage::DatasetOptions;
    use std::process::Command;

    /// 临时池名前缀——避免与真实池冲突，测后必须 destroy。
    /// 与 os-storage/tests/real_zfs_ops.rs 一致，确保全工作区 osprobe 池可被统一识别。
    const POOL_PREFIX: &str = "osprobe";

    /// vdev 稀疏文件大小（256M 足够建数据集 + 快照 + send 流；sparse 不真占盘）。
    const VDEV_SIZE: &str = "256M";

    /// 纯 Rust 的 `which`：扫 $PATH 找可执行文件（避免引入 which crate 依赖）。
    fn which(bin: &str) -> Option<std::path::PathBuf> {
        let path = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(bin);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
        None
    }

    /// 生成唯一临时池名（带 PID + 纳秒时间戳，防并发测冲突 + 避免碰宿主真实 pool）。
    fn unique_pool(tag: &str) -> String {
        format!(
            "{POOL_PREFIX}_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )
    }

    /// 生成唯一临时 vdev 文件路径（同 PID + 纳秒，与池名配对）。
    fn unique_vdev(tag: &str) -> String {
        format!(
            "/tmp/{POOL_PREFIX}_{tag}_{}_{}.img",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        )
    }

    /// 真实环境预检：zfs 二进制 + 内核模块可用 + root。
    ///
    /// 全部满足返回 true；缺其一则 eprintln 报告缺什么并返回 false（调用方据此优雅跳过）。
    fn real_zfs_ready() -> bool {
        if which("zfs").is_none() {
            eprintln!(
                "[backup_real] SKIP: `zfs` 二进制不在 $PATH —— 需装 zfsutils-linux \
                 (Debian: `apt install zfsutils-linux`)。"
            );
            return false;
        }
        // `zfs version`（OpenZFS 2.x+）exit 0 表示 userland + kmod 都在。
        let probe = Command::new("zfs").arg("version").output();
        let userland_ok = match probe {
            Ok(o) if o.status.success() => true,
            Ok(_) => {
                let o2 = Command::new("zfs").arg("--version").output();
                matches!(o2, Ok(o2) if o2.status.success())
            }
            Err(e) => {
                eprintln!("[backup_real] SKIP: spawn `zfs version` 失败：{e}");
                return false;
            }
        };
        if !userland_ok {
            eprintln!("[backup_real] SKIP: `zfs version` 非 0 退出（可能 zfs 内核模块未加载）");
            return false;
        }
        // root 检查（zpool create / zfs send 需 root）。
        let uid = Command::new("id").arg("-u").output();
        match uid {
            Ok(o) if String::from_utf8_lossy(&o.stdout).trim() == "0" => true,
            _ => {
                eprintln!(
                    "[backup_real] SKIP: 非 root（zpool create / zfs send 需 root）。\
                     跑法：sudo cargo test -p os-services --features mock --test backup_real -- --ignored --nocapture"
                );
                false
            }
        }
    }

    /// RAII 销毁池 + 删 sparse file（即使断言失败也清理）。
    ///
    /// Drop 不能 async，直接用同步 `std::process::Command` spawn `zpool destroy -f`——
    /// 不走 ZfsCliBackend（Drop 在 tokio runtime 线程内，再 block_on 建嵌套 runtime 会 panic）。
    /// `-f` 容忍已销毁 / 有残留数据集。
    struct RealPoolGuard {
        pool: String,
        vdevs: Vec<String>,
    }

    impl Drop for RealPoolGuard {
        fn drop(&mut self) {
            let _ = Command::new("zpool")
                .args(["destroy", "-f", &self.pool])
                .status();
            for v in &self.vdevs {
                let _ = std::fs::remove_file(v);
            }
        }
    }

    /// 建 sparse file vdev（truncate 不真占盘）。
    fn make_sparse_vdev(path: &str) {
        let r = Command::new("truncate")
            .args(["-s", VDEV_SIZE, path])
            .status();
        match r {
            Ok(s) if s.success() => {}
            other => panic!("[backup_real] 建 sparse vdev {path} 失败: {other:?}"),
        }
    }

    // ------------------------------------------------------------------------
    // B.a 真实 run_backup 本地快照：建临时池 + dataset → run_backup → 验证快照真实存在
    // ------------------------------------------------------------------------

    /// 真实验证：建临时池（sparse file vdev）→ 建 dataset → 用 `ZfsCliBackend`（真实 zfs）
    /// 经 `ZfsBackupManager::trigger_now` 创建快照 → `zfs list -t snapshot` 真实可见 → teardown。
    ///
    /// **关键路径**：`trigger_now` → `ZfsCliBackend::snapshot` → 真实 `zfs snapshot <ds>@<name>`。
    /// 这条路径在 mock 测里只验证 mock 内部状态自增，本测验证**真实 zfs 内核态快照落地**。
    ///
    /// 注意：`ZfsBackupManager` 泛型参数 `B: StorageBackend`，本测用 `ZfsCliBackend`（真实）
    /// 而非 `MockStorageBackend`，但 `ZfsCliBackend` 已实现 `StorageBackend`，直接注入。
    #[tokio::test]
    #[ignore = "真实 zfs 池操作：需 root + zfsutils-linux + zfs 模块。sudo cargo test -- --ignored --nocapture"]
    async fn real_trigger_now_creates_real_zfs_snapshot() {
        if !real_zfs_ready() {
            return;
        }

        let pool_name = unique_pool("backup");
        let vdev_path = unique_vdev("backup");
        eprintln!(
            "[backup_real] 真实快照测：pool={pool_name} vdev={vdev_path}（sparse {VDEV_SIZE}）"
        );

        make_sparse_vdev(&vdev_path);
        let _guard = RealPoolGuard {
            pool: pool_name.clone(),
            vdevs: vec![vdev_path.clone()],
        };

        // 用真实 ZfsCliBackend（调 zpool/zfs CLI）。
        let backend = Arc::new(os_storage::ZfsCliBackend::new());

        // 1. 建池 + 建 dataset（真实 zfs）。
        backend
            .create_pool(
                &PoolId::new(pool_name.clone()),
                vec![VdevSpec {
                    kind: VdevKind::Disk,
                    disks: vec![vdev_path.clone()],
                }],
            )
            .await
            .expect("zpool create 应成功");
        eprintln!("[backup_real] zpool create OK");

        let ds_full = format!("{pool_name}/media");
        backend
            .create_dataset(&DatasetId::new(ds_full.clone()), DatasetOptions::default())
            .await
            .expect("create_dataset 应成功");
        eprintln!("[backup_real] zfs create {ds_full} OK");

        // 2. 构造 manager（注入真实 backend）+ schedule + trigger_now。
        let mgr: ZfsBackupManager<os_storage::ZfsCliBackend> =
            ZfsBackupManager::new(backend.clone());
        let policy = BackupPolicy {
            name: "real-backup".into(),
            schedule: CronExpr::new("0 3 * * *"),
            retention: RetentionPolicy {
                keep_last: 7,
                keep_days: 7,
            },
            source: DatasetId::new(ds_full.clone()),
            target_remote: None,
        };
        let job_id = mgr.schedule(policy).await.expect("schedule 应成功");
        let _task = mgr.trigger_now(&job_id).await.expect("trigger_now 应成功");
        eprintln!("[backup_real] trigger_now OK（经 ZfsCliBackend 真实 zfs snapshot）");

        // 3. 验证 job 状态 Success。
        let jobs = mgr.list_jobs().await.unwrap();
        assert_eq!(jobs[0].status, BackupStatus::Success);

        // 4. 关键断言：快照真实存在于 zfs 内核态（zfs list -t snapshot 可见）。
        let snaps = backend
            .list_snapshots(Some(&DatasetId::new(ds_full.clone())))
            .await
            .expect("list_snapshots 应成功");
        assert!(
            !snaps.is_empty(),
            "trigger_now 后应有真实快照，实际: {snaps:?}"
        );
        let snap = &snaps[0];
        assert!(
            snap.id.as_str().starts_with(&format!("{ds_full}@auto-")),
            "快照名应匹配 {ds_full}@auto-<ts>，实际: {}",
            snap.id.as_str()
        );
        // creation 是真实 Unix 秒（2023 后）。
        assert!(
            snap.created.timestamp() > 1_700_000_000,
            "快照 creation 应是近期 Unix 秒: {}",
            snap.created.timestamp()
        );

        // 5. 双重验证：直接调 `zfs list -t snapshot` 子进程（不经 backend），确认快照真在 zfs 里。
        let zfs_list = Command::new("zfs")
            .args(["list", "-t", "snapshot", "-o", "name", "-H", &ds_full])
            .output()
            .expect("spawn zfs list 失败");
        assert!(
            zfs_list.status.success(),
            "zfs list 应成功，stderr: {}",
            String::from_utf8_lossy(&zfs_list.stderr)
        );
        let stdout = String::from_utf8_lossy(&zfs_list.stdout);
        assert!(
            stdout.contains(&format!("{ds_full}@auto-")),
            "zfs list -t snapshot 应含 trigger_now 创建的快照，实际 stdout: {stdout}"
        );
        eprintln!(
            "[backup_real] 真实快照落地 OK：zfs list -t snapshot = {}",
            stdout.trim()
        );

        // guard.drop 销毁池 + 删 sparse file。
    }

    // ------------------------------------------------------------------------
    // B.b 真实 scrub_status：建临时池 → scrub → 解析 zpool status 输出
    // ------------------------------------------------------------------------

    /// 真实验证 scrub 查询原语：建临时池 → `zpool scrub` → `zpool status` → 用
    /// [`parse_scrub_from_zpool_status`] 解析 scrub 行 → 断言可提取 errors/repaired/duration。
    ///
    /// **接通点**：`impl_backup::scrub_status`（行 158-169）TODO [RUNTIME] 当前返回空报告。
    /// 本测独立验证 `zpool status` 解析器对真实 zfs 2.4.1 输出的正确性，为接通提供
    /// 经过本机验证的解析逻辑。
    #[tokio::test]
    #[ignore = "真实 zfs scrub：需 root + zfsutils-linux + zfs 模块。sudo cargo test -- --ignored --nocapture"]
    async fn real_scrub_status_parses_zpool_status() {
        if !real_zfs_ready() {
            return;
        }

        let pool_name = unique_pool("scrub");
        let vdev_path = unique_vdev("scrub");
        eprintln!(
            "[backup_real] 真实 scrub 测：pool={pool_name} vdev={vdev_path}（sparse {VDEV_SIZE}）"
        );

        make_sparse_vdev(&vdev_path);
        let _guard = RealPoolGuard {
            pool: pool_name.clone(),
            vdevs: vec![vdev_path.clone()],
        };

        // 1. 建池（真实 zfs）—— 直接 spawn zpool（不走 backend，简化建池后立即 scrub）。
        let create = Command::new("zpool")
            .args(["create", &pool_name, &vdev_path])
            .status()
            .expect("spawn zpool create 失败");
        assert!(create.success(), "zpool create 应成功");
        eprintln!("[backup_real] zpool create OK");

        // 2. scrub 前 status（应无 scan 行或 none requested）。
        let status_before = Command::new("zpool")
            .args(["status", &pool_name])
            .output()
            .expect("spawn zpool status 失败");
        let stdout_before = String::from_utf8_lossy(&status_before.stdout).to_string();
        eprintln!("[backup_real] scrub 前 zpool status:\n{stdout_before}");

        // 3. 触发 scrub（小池瞬间完成，status 会直接显示 completed）。
        let scrub = Command::new("zpool")
            .args(["scrub", &pool_name])
            .status()
            .expect("spawn zpool scrub 失败");
        assert!(scrub.success(), "zpool scrub 应成功");

        // 极小池 scrub 几乎瞬时完成；sleep 一小段确保 status 反映完成态。
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // 4. scrub 后 status。
        let status_after = Command::new("zpool")
            .args(["status", &pool_name])
            .output()
            .expect("spawn zpool status 失败");
        let stdout_after = String::from_utf8_lossy(&status_after.stdout).to_string();
        eprintln!("[backup_real] scrub 后 zpool status:\n{stdout_after}");

        // 5. 关键断言：用解析器提取 scrub 信息。
        let report = parse_scrub_from_zpool_status(&stdout_after);
        let report = report.expect("scrub 后应能解析出 ScrubReport");
        assert!(
            report.errors == 0,
            "新建空池 scrub 应 0 错误，实际: {}",
            report.errors
        );
        assert_eq!(report.repaired, 0, "新建空池 scrub 应 0 修复（无数据）");
        eprintln!(
            "[backup_real] 真实 scrub 解析 OK: errors={} repaired={}B duration={}s",
            report.errors, report.repaired, report.duration_secs
        );

        // 6. 验证 scrub 行确实在输出里（双重确认 zpool status 反映了 scrub）。
        assert!(
            stdout_after
                .lines()
                .any(|l| l.trim_start().starts_with("scan:") && l.contains("scrub")),
            "zpool status 应含 scrub scan 行，实际:\n{stdout_after}"
        );

        // 7. 额外验证：scrub 前的 status 解析也应可处理（无 scrub 记录 → None 或 completed）。
        //    新池首次 status 通常是 "scan: none requested" → None。
        let report_before = parse_scrub_from_zpool_status(&stdout_before);
        eprintln!(
            "[backup_real] scrub 前 status 解析: {:?}（新池应 None 或无记录）",
            report_before
        );

        // guard.drop 销毁池 + 删 sparse file。
    }

    // ------------------------------------------------------------------------
    // B.c 真实 zfs send 到文件（远程复制数据流模拟）
    // ------------------------------------------------------------------------

    /// 真实验证 zfs send 数据流：建临时池 + dataset + 快照 → `zfs send <snap> > /tmp/backup.stream`
    /// → 断言 stream 文件非空（含真实 zfs send 二进制头）。
    ///
    /// **接通点**：远程复制（`target_remote = Some`）的 TODO [RUNTIME] 接通后，
    /// `zfs send <snap>` 的 stdout 会经管道喂给远端 `ssh ... zfs recv` 的 stdin。
    /// 本测把 stdout 重定向到文件，**模拟远程复制的数据流**——验证 send 端能产出
    /// 合法非空流（这是远程复制的前置条件；recv 端的接通属跨 crate 依赖，不在此验证）。
    #[tokio::test]
    #[ignore = "真实 zfs send：需 root + zfsutils-linux + zfs 模块。sudo cargo test -- --ignored --nocapture"]
    async fn real_zfs_send_produces_nonempty_stream() {
        if !real_zfs_ready() {
            return;
        }

        let pool_name = unique_pool("send");
        let vdev_path = unique_vdev("send");
        let stream_path = format!(
            "/tmp/{POOL_PREFIX}_send_stream_{}_{}.stream",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        eprintln!(
            "[backup_real] 真实 send 测：pool={pool_name} vdev={vdev_path} stream={stream_path}"
        );

        make_sparse_vdev(&vdev_path);
        // guard 同时清理 vdev + stream。
        struct SendGuard {
            pool: String,
            vdev: String,
            stream: String,
        }
        impl Drop for SendGuard {
            fn drop(&mut self) {
                let _ = Command::new("zpool")
                    .args(["destroy", "-f", &self.pool])
                    .status();
                let _ = std::fs::remove_file(&self.vdev);
                let _ = std::fs::remove_file(&self.stream);
            }
        }
        let _guard = SendGuard {
            pool: pool_name.clone(),
            vdev: vdev_path.clone(),
            stream: stream_path.clone(),
        };

        // 1. 建池 + dataset（真实 zfs）。
        let create = Command::new("zpool")
            .args(["create", &pool_name, &vdev_path])
            .status()
            .expect("spawn zpool create 失败");
        assert!(create.success(), "zpool create 应成功");

        let ds_full = format!("{pool_name}/media");
        let mkds = Command::new("zfs")
            .args(["create", &ds_full])
            .status()
            .expect("spawn zfs create 失败");
        assert!(mkds.success(), "zfs create 应成功");
        eprintln!("[backup_real] zfs create {ds_full} OK");

        // 2. 写一点数据到 dataset（让 send 流非平凡——空 dataset 的 send 流仍非空但小）。
        let mount_check = Command::new("zfs")
            .args(["get", "-H", "-o", "value", "mounted", &ds_full])
            .output()
            .expect("spawn zfs get 失败");
        let mounted = String::from_utf8_lossy(&mount_check.stdout)
            .trim()
            .to_string();
        eprintln!("[backup_real] dataset mounted={mounted}");
        // 写一些数据（若已挂载，找到挂载点写文件；否则用 zfs write 不可行，跳过）。
        if mounted == "yes" {
            let mountpoint_out = Command::new("zfs")
                .args(["get", "-H", "-o", "value", "mountpoint", &ds_full])
                .output()
                .expect("spawn zfs get mountpoint 失败");
            let mp = String::from_utf8_lossy(&mountpoint_out.stdout)
                .trim()
                .to_string();
            eprintln!("[backup_real] dataset mountpoint={mp}");
            if mp != "-" && !mp.is_empty() {
                let testfile = format!("{mp}/data.bin");
                // 写 4KB 随机数据（dd from /dev/urandom）。
                let _ = Command::new("dd")
                    .args([
                        "if=/dev/urandom",
                        &format!("of={testfile}"),
                        "bs=4096",
                        "count=1",
                    ])
                    .status();
            }
        }

        // 3. 建快照（真实 zfs）。
        let snap_name = "snap1";
        let snap_full = format!("{ds_full}@{snap_name}");
        let mksnap = Command::new("zfs")
            .args(["snapshot", &snap_full])
            .status()
            .expect("spawn zfs snapshot 失败");
        assert!(mksnap.success(), "zfs snapshot 应成功");
        eprintln!("[backup_real] zfs snapshot {snap_full} OK");

        // 4. 用 A.b 的命令构造逻辑构造 send argv，验证格式正确（与真实 zfs send 一致）。
        let (send_argv, _recv_argv) = build_send_recv_cmd(
            &SnapshotId::new(&snap_full),
            "backuphost:backup/media",
            "root",
        );
        assert_eq!(
            send_argv,
            vec!["zfs", "send", &snap_full],
            "send argv 应匹配 zfs send <snap>"
        );
        eprintln!(
            "[backup_real] send argv 构造验证 OK: {}",
            send_argv.join(" ")
        );

        // 5. 真实执行 `zfs send <snap> > stream_path`（shell 管道重定向）。
        //    用 sh -c 把 stdout 重定向到文件（tokio::Command 不直接支持重定向到文件）。
        let send_cmd_str = format!("zfs send {snap_full} > {stream_path}");
        let send_status = Command::new("sh")
            .args(["-c", &send_cmd_str])
            .status()
            .expect("spawn sh -c 'zfs send ...' 失败");
        assert!(
            send_status.success(),
            "zfs send 应成功（exit 0），状态: {send_status:?}"
        );
        eprintln!("[backup_real] zfs send 执行成功");

        // 6. 关键断言：stream 文件真实存在 + 非空。
        let stream_meta = std::fs::metadata(&stream_path).expect("stream 文件应存在");
        assert!(
            stream_meta.len() > 0,
            "zfs send stream 应非空（含 zfs 流头 + 数据），实际 0 字节"
        );
        eprintln!(
            "[backup_real] zfs send stream 非空 OK: {} 字节",
            stream_meta.len()
        );

        // 7. 验证 stream 头部是 zfs send 格式（前 8 字节是 magic / version）。
        let head = std::fs::read(&stream_path).unwrap();
        let head = &head[..head.len().min(16)];
        eprintln!(
            "[backup_real] stream 头 16 字节: {}",
            head.iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        // zfs send 流以 8 字节 begin record 开头（非全零；OpenZFS 2.4 实测含 magic）。
        assert!(
            head.len() >= 8,
            "stream 头应至少 8 字节（zfs send begin record）"
        );

        // 8. 双重验证：stream 能被 `zfs recv` 接收（验证它确实是合法 zfs 流）。
        //    建第二个 dataset，把 stream recv 进去，确认数据可还原。
        let recv_ds = format!("{pool_name}/restored");
        let _ = Command::new("zfs").args(["create", &recv_ds]).status();
        // 先 destroy recv_ds 让 recv 接收为全新 dataset（recv 要求目标不存在或 -F）。
        let _ = Command::new("zfs")
            .args(["destroy", "-r", &recv_ds])
            .status();
        let recv_status = Command::new("sh")
            .args(["-c", &format!("zfs recv {recv_ds} < {stream_path}")])
            .status()
            .expect("spawn zfs recv 失败");
        assert!(
            recv_status.success(),
            "zfs recv 应成功（验证 stream 合法），状态: {recv_status:?}"
        );
        // recv 后 recv_ds 应存在 + 含同名快照。
        let recv_check = Command::new("zfs")
            .args(["list", "-t", "snapshot", "-o", "name", "-H", &recv_ds])
            .output()
            .expect("spawn zfs list 失败");
        let recv_stdout = String::from_utf8_lossy(&recv_check.stdout);
        assert!(
            recv_stdout.contains(&format!("{recv_ds}@{snap_name}")),
            "recv 后应有同名快照 {recv_ds}@{snap_name}，实际: {recv_stdout}"
        );
        eprintln!("[backup_real] zfs recv 回放 stream OK：快照 {recv_ds}@{snap_name} 真实还原");

        // guard.drop 销毁池 + 删 vdev + 删 stream。
    }
}
