//! os-storage `ZfsCliBackend` 真实池操作实跑验证（sparse file vdev）。
//!
//! 对应 docs/SANDBOX.md §5「应入沙箱测试清单」的 zfs 项。本测**自包含**：
//! 不依赖任何环境变量，自己用 `truncate` 建稀疏文件 vdev，自己 `zpool create`
//! 建临时池，跑完 create/list/snapshot/destroy 全链后 RAII 销毁池 + 删文件，
//! 不残留任何状态、不碰 /dev/sdX 真实磁盘。
//!
//! ## 为什么自包含而非读环境变量
//! `backend_impl::real_zfs_sandbox_tests` 里的老测要外部预设 `OS_TEST_VDEV` /
//! `OS_TEST_POOL`，手动跑门槛高。本测把 sparse file 制作 + 建池 + teardown 全包，
//! `sudo cargo test -p os-storage --test real_zfs_ops -- --ignored` 一条命令即可真跑，
//! 降低「真实跑一次 zfs」的摩擦（沙箱镜像 / 本机 root + zfs 模块即可）。
//!
//! ## 跑法
//! ```bash
//! # 需 root（zpool create 写内核状态）+ zfs 模块加载 + zfsutils-linux 装好。
//! sudo cargo test -p os-storage --test real_zfs_ops -- --ignored --nocapture
//! ```
//! 非 root / 无 zfs 二进制 / 模块未加载：**优雅跳过**（eprintln 报告缺什么，不 panic），
//! 不污染默认 `cargo test` 套件（`#[ignore]` 默认不执行）。
//!
//! ## 红线
//! 只用 sparse file vdev（`/tmp/osprobe-<pid>-<ts>.img`），**绝不碰 /dev/sdX**。
//! 池名加 PID+纳秒时间戳前缀防并发测冲突；teardown 用 RAII guard 保证即使断言失败也清理。

#![cfg(feature = "mock")] // 与 backend_impl::real_zfs_sandbox_tests 保持一致：沙箱测在 mock feature 下编译

use os_core::{DatasetId, PoolId, SnapshotId};
use os_storage::model::VdevKind;
use os_storage::{DatasetOptions, StorageBackend, StorageError, VdevSpec, ZfsCliBackend};
use std::process::Command;

/// 临时池名前缀——避免与真实池冲突，测后必须 destroy。
const POOL_PREFIX: &str = "osprobe";

/// vdev 稀疏文件大小（1G 足够建数据集做快照；sparse 不真占盘）。
const VDEV_SIZE: &str = "1G";

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

/// 生成唯一临时池名（带 PID + 纳秒时间戳，防并发测冲突）。
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
            "[real_zfs_ops] SKIP: `zfs` 二进制不在 $PATH —— 需装 zfsutils-linux \
             (Debian: `apt install zfsutils-linux`)。详见 docs/SANDBOX.md §5。"
        );
        return false;
    }
    // `zfs version`（新版）或 `zfs --version`（旧版）exit 0 表示 userland + kmod 都在。
    // 优先 `version`（OpenZFS 2.x+ 标准），失败回退 `--version`。
    let probe = Command::new("zfs").arg("version").output();
    let ok = match probe {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            // 回退 --version
            let o2 = Command::new("zfs").arg("--version").output();
            match o2 {
                Ok(o2) if o2.status.success() => true,
                _ => {
                    eprintln!(
                        "[real_zfs_ops] SKIP: `zfs version` 退出码非 0（可能内核 ZFS 模块\
                         未加载或非 root）。version stderr: {}",
                        String::from_utf8_lossy(&o.stderr)
                    );
                    false
                }
            }
        }
        Err(e) => {
            eprintln!("[real_zfs_ops] SKIP: spawn `zfs version` 失败：{e}");
            return false;
        }
    };
    if !ok {
        return false;
    }
    // root 检查（zpool create 需 root）。geteuid 经 libc 太重，直接看 `id -u`。
    let uid = Command::new("id").arg("-u").output();
    match uid {
        Ok(o) if String::from_utf8_lossy(&o.stdout).trim() == "0" => true,
        _ => {
            eprintln!(
                "[real_zfs_ops] SKIP: 非 root（zpool create 需 root）。\
                 跑法：sudo cargo test -p os-storage --test real_zfs_ops -- --ignored"
            );
            false
        }
    }
}

/// RAII 销毁池 + 删 sparse file（即使断言失败也清理）。
///
/// Drop 不能 async，用一次性 current_thread runtime 阻塞执行 destroy_pool；
/// 失败（池已销毁 / 文件已删）静默忽略——teardown 本就是「尽力清理」。
struct RealPoolGuard {
    pool: String,
    vdev: String,
}

impl Drop for RealPoolGuard {
    fn drop(&mut self) {
        // 销毁池：用同步 std::process::Command 直接调 `zpool destroy -f`（不走
        // ZfsCliBackend）。原因：Drop 在 tokio::test 的 runtime 线程内执行，此时再
        // block_on 建嵌套 runtime 会 panic（"Cannot start a runtime from within a
        // runtime"）。teardown 只需把池/文件清掉，直接 spawn zpool 子进程最简单可靠。
        // -f 容忍已销毁 / 有残留数据集（`-r` 由 destroy 自带递归销毁子项）。
        let _ = Command::new("zpool")
            .args(["destroy", "-f", &self.pool])
            .status();
        // 删 sparse file（容忍已删）。
        let _ = std::fs::remove_file(&self.vdev);
    }
}

/// 真实跑通：sparse file vdev → zpool create → zfs create ds → snapshot → list → destroy 全链。
///
/// 断言：
/// - `create_pool` 回读成功，pool 解析正确（id / 健康状态）。
/// - `list_pools` 能看到新池。
/// - `create_dataset` 回读成功，dataset 解析正确（id / pool 派生）。
/// - `snapshot` 回读成功，snapshot 解析正确（id / dataset / creation）。
/// - `list_snapshots` 能看到快照。
/// - `destroy_snapshot` 后 list_snapshots 不再有它。
/// - `destroy_dataset` 后 list_datasets 不再有它。
/// - `destroy_pool` 后 list_pools 不再有它。
#[tokio::test]
#[ignore = "真实 zfs 池操作：需 root + zfsutils-linux + zfs 模块。跑法：sudo cargo test -p os-storage --test real_zfs_ops -- --ignored --nocapture"]
async fn real_sparse_file_pool_dataset_snapshot_lifecycle() {
    if !real_zfs_ready() {
        return;
    }

    let pool_name = unique_pool("lifecycle");
    let vdev_path = unique_vdev("lifecycle");
    eprintln!("[real_zfs_ops] 池={pool_name} vdev={vdev_path}（sparse {VDEV_SIZE}）");

    // —— 建稀疏文件 vdev（truncate 不真占盘）——
    let truncate = Command::new("truncate")
        .args(["-s", VDEV_SIZE, &vdev_path])
        .status();
    match truncate {
        Ok(s) if s.success() => {}
        other => panic!("[real_zfs_ops] 建 sparse vdev 失败: {other:?}"),
    }

    let _guard = RealPoolGuard {
        pool: pool_name.clone(),
        vdev: vdev_path.clone(),
    };

    let backend = ZfsCliBackend::new();

    // —— 1. create_pool（sparse file 作为 Disk vdev）——
    let pool = backend
        .create_pool(
            &PoolId::new(pool_name.clone()),
            vec![VdevSpec {
                kind: VdevKind::Disk,
                disks: vec![vdev_path.clone()],
            }],
        )
        .await
        .expect("zpool create + 回读应成功");
    assert_eq!(pool.id.as_str(), pool_name);
    // 1G sparse → 实际容量约 960M（zfs 留一点元数据头），total_bytes 应 > 0。
    assert!(
        pool.capacity.total_bytes > 900_000_000,
        "池容量应近 1G（sparse vdev），实际: {}",
        pool.capacity.total_bytes
    );
    eprintln!(
        "[real_zfs_ops] create_pool OK: capacity={}/{}, health={:?}",
        pool.capacity.used_bytes, pool.capacity.total_bytes, pool.health
    );

    // —— 2. list_pools 能看到新池 ——
    let pools = backend.list_pools().await.expect("list_pools 应成功");
    let found = pools.iter().find(|p| p.id.as_str() == pool_name);
    assert!(found.is_some(), "新池应在 list_pools 结果中: {pools:?}");
    eprintln!("[real_zfs_ops] list_pools OK（{} 个池）", pools.len());

    // —— 3. create_dataset ——
    let ds_full = format!("{pool_name}/media");
    let ds = backend
        .create_dataset(&DatasetId::new(ds_full.clone()), DatasetOptions::default())
        .await
        .expect("create_dataset 应成功");
    assert_eq!(ds.id.as_str(), ds_full);
    assert_eq!(ds.pool.as_str(), pool_name);
    eprintln!(
        "[real_zfs_ops] create_dataset OK: id={} mounted={}",
        ds.id, ds.mounted
    );

    // list_datasets 能看到（zfs list -t filesystem,volume 会同时返回 pool 本身 + ds）。
    let datasets = backend
        .list_datasets(Some(&PoolId::new(pool_name.clone())))
        .await
        .expect("list_datasets 应成功");
    assert!(
        datasets.iter().any(|d| d.id.as_str() == ds_full),
        "新数据集应在 list_datasets 结果中: {datasets:?}"
    );
    eprintln!(
        "[real_zfs_ops] list_datasets OK（{} 行，含 pool 本身 + 子数据集）",
        datasets.len()
    );

    // —— 4. snapshot ——
    let snap_name = "snap1";
    let snap = backend
        .snapshot(&DatasetId::new(ds_full.clone()), snap_name)
        .await
        .expect("snapshot 应成功");
    let snap_full = format!("{ds_full}@{snap_name}");
    assert_eq!(snap.id.as_str(), snap_full);
    assert_eq!(snap.dataset.as_str(), ds_full);
    // creation 是真实 Unix 秒（接近现在），应 > 1_700_000_000（2023 后）。
    assert!(
        snap.created.timestamp() > 1_700_000_000,
        "快照 creation 应是近期 Unix 秒: {}",
        snap.created.timestamp()
    );
    eprintln!(
        "[real_zfs_ops] snapshot OK: id={} created={}",
        snap.id,
        snap.created.timestamp()
    );

    // —— 5. list_snapshots 能看到快照 ——
    let snaps = backend
        .list_snapshots(Some(&DatasetId::new(ds_full.clone())))
        .await
        .expect("list_snapshots 应成功");
    assert!(
        snaps.iter().any(|s| s.id.as_str() == snap_full),
        "新快照应在 list_snapshots 结果中: {snaps:?}"
    );
    eprintln!("[real_zfs_ops] list_snapshots OK（{} 个快照）", snaps.len());

    // —— 6. destroy_snapshot 后消失 ——
    backend
        .destroy_snapshot(&SnapshotId::new(snap_full.clone()))
        .await
        .expect("destroy_snapshot 应成功");
    let snaps = backend
        .list_snapshots(Some(&DatasetId::new(ds_full.clone())))
        .await
        .expect("list_snapshots 应成功");
    assert!(
        !snaps.iter().any(|s| s.id.as_str() == snap_full),
        "destroy 后快照不应再出现: {snaps:?}"
    );
    eprintln!("[real_zfs_ops] destroy_snapshot OK（快照已消失）");

    // —— 7. destroy_dataset 后消失（-r 递归，即使留了快照也能销毁）——
    backend
        .destroy_dataset(&DatasetId::new(ds_full.clone()))
        .await
        .expect("destroy_dataset 应成功");
    let datasets = backend
        .list_datasets(Some(&PoolId::new(pool_name.clone())))
        .await
        .expect("list_datasets 应成功");
    assert!(
        !datasets.iter().any(|d| d.id.as_str() == ds_full),
        "destroy 后数据集不应再出现: {datasets:?}"
    );
    eprintln!("[real_zfs_ops] destroy_dataset OK（数据集已消失）");

    // —— 8. destroy_pool 后消失（显式销毁，不靠 guard）——
    backend
        .destroy_pool(&PoolId::new(pool_name.clone()))
        .await
        .expect("destroy_pool 应成功");
    let pools = backend.list_pools().await.expect("list_pools 应成功");
    assert!(
        !pools.iter().any(|p| p.id.as_str() == pool_name),
        "destroy 后池不应再出现: {pools:?}"
    );
    eprintln!("[real_zfs_ops] destroy_pool OK（池已消失）—— 全链通过");

    // guard.drop 会再尝试 destroy_pool（已销毁，静默）+ 删 sparse file。
    // 手动把 sparse file 删了（guard 也会删，幂等）。
    let _ = std::fs::remove_file(&vdev_path);
}

/// 真实验证错误分类：对已存在的池 `create_pool` 应映射成 `PoolExists`。
///
/// 这条路径在 fixture 单测里覆盖过（`create_pool_already_exists_maps_to_pool_exists`），
/// 但那是注入的 stderr；本测用真实 zpool 产生真实 stderr，验证 OpenZFS 2.4 的
/// 「already exists」关键词仍被 `classify_err` 正确识别（防止上游 zfs 改了措辞）。
#[tokio::test]
#[ignore = "真实 zfs 错误分类：需 root + zfsutils-linux + zfs 模块。跑法：sudo cargo test -p os-storage --test real_zfs_ops -- --ignored --nocapture"]
async fn real_pool_exists_error_classification() {
    if !real_zfs_ready() {
        return;
    }

    let pool_name = unique_pool("exists");
    let vdev_a = unique_vdev("exists_a");
    // 第二次 create 必须用**不同的** vdev 文件——OpenZFS 2.4 在复用同一 sparse vdev 时
    // 报的是 "invalid vdev specification ... is part of active pool"（映射 InvalidVdev），
    // 而非 "pool already exists"。要触发真实 PoolExists 路径，需同池名 + 不同 vdev。
    let vdev_b = unique_vdev("exists_b");
    eprintln!("[real_zfs_ops] 池={pool_name} vdev_a={vdev_a} vdev_b={vdev_b}");

    // 建两个 sparse vdev
    for v in [&vdev_a, &vdev_b] {
        let truncate = Command::new("truncate").args(["-s", VDEV_SIZE, v]).status();
        match truncate {
            Ok(s) if s.success() => {}
            other => panic!("[real_zfs_ops] 建 sparse vdev {v} 失败: {other:?}"),
        }
    }

    let _guard = RealPoolGuard {
        pool: pool_name.clone(),
        // guard 销毁池后顺手删两个 vdev；vdev_b 未被任何池用，直接删即可。
        vdev: vdev_a.clone(),
    };

    let backend = ZfsCliBackend::new();

    // 第一次 create 成功（用 vdev_a）。
    backend
        .create_pool(
            &PoolId::new(pool_name.clone()),
            vec![VdevSpec {
                kind: VdevKind::Disk,
                disks: vec![vdev_a.clone()],
            }],
        )
        .await
        .expect("首次 zpool create 应成功");

    // 第二次 create 同名池 + 不同 vdev → 真实 stderr 含 "already exists" → 映射 PoolExists。
    let err = backend
        .create_pool(
            &PoolId::new(pool_name.clone()),
            vec![VdevSpec {
                kind: VdevKind::Disk,
                disks: vec![vdev_b.clone()],
            }],
        )
        .await
        .expect_err("二次 create 同名池应失败");
    assert!(
        matches!(err, StorageError::PoolExists(_)),
        "应映射为 PoolExists（真实 stderr 含 'already exists'），实际: {err:?}"
    );
    eprintln!("[real_zfs_ops] PoolExists 分类 OK: {err}");

    // destroy_pool 显式清理（guard 兜底 vdev_a）。
    backend
        .destroy_pool(&PoolId::new(pool_name.clone()))
        .await
        .expect("destroy_pool 应成功");
    let _ = std::fs::remove_file(&vdev_a);
    let _ = std::fs::remove_file(&vdev_b);
}
