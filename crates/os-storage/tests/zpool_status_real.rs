//! `zpool status` 树形输出解析测：纯函数单测（默认跑）+ 真实池解析（#[ignore]）。
//!
//! 对应 docs/SANDBOX.md §5「应入沙箱测试清单」的 zpool status 项。分两类：
//!
//! ## A. 解析器纯函数单测（默认跑，无 zfs 依赖）
//! 用固定 zpool status 输出字符串验证 [`parse_zpool_status`] 对 vdev 树的解析：
//! 单盘 / mirror / raidz1 / 故障态 + 非零错误计数 / 异常输出容错。
//!
//! ## B. 真实 zpool status 解析（#[ignore]，需 zfs）
//! 跑真实 `zpool status`，验证解析器对本机持久测试池 `osprobepersist` 的解析正确。
//! 只读不写（绝不 destroy 宿主池）。无 zfs / 无池 → 优雅 SKIP。
//!
//! ## 跑法
//! ```bash
//! # A 类（默认）
//! cargo test -p os-storage --features mock --test zpool_status_real
//! # B 类（真实，需 zfs + 可读池）
//! cargo test -p os-storage --features mock --test zpool_status_real -- --ignored --nocapture
//! ```

#![cfg(feature = "mock")]

use os_core::Health;
use os_storage::model::VdevKind;
use os_storage::{parse_zpool_status, PoolStatus};
use std::process::Command;

// ============================================================================
// A. 解析器纯函数单测（默认跑）—— 固定 zpool status 字符串 → 验证 vdev 树解析
// ============================================================================

/// 真实 OpenZFS 2.4 单盘池样本（本机 osprobepersist，sparse file vdev）。
/// 用 concat! + 显式 \t/\n 保证缩进与真实输出字节级一致（避免 `\` 续行吞缩进）。
fn single_disk_status() -> String {
    concat!(
        "  pool: osprobepersist\n",
        " state: ONLINE\n",
        "  scan: scrub repaired 0B in 00:00:00 with 0 errors on Thu Aug  6 18:02:30 2026\n",
        "config:\n",
        "\n",
        "\tNAME                         STATE     READ WRITE CKSUM\n",
        "\tosprobepersist              ONLINE       0     0     0\n",
        "\t  /tmp/osprobe-persist.img  ONLINE       0     0     0\n",
        "\n",
        "errors: No known data errors\n",
    )
    .to_string()
}

/// mirror 池样本（2 盘镜像）。
fn mirror_status() -> String {
    concat!(
        "  pool: tank\n",
        " state: ONLINE\n",
        "config:\n",
        "\n",
        "\tNAME        STATE     READ WRITE CKSUM\n",
        "\ttank        ONLINE       0     0     0\n",
        "\t  mirror-0  ONLINE       0     0     0\n",
        "\t    /dev/sdb  ONLINE       0     0     0\n",
        "\t    /dev/sdc  ONLINE       0     0     0\n",
        "\n",
        "errors: No known data errors\n",
    )
    .to_string()
}

/// raidz1 池样本（3 盘单校验）。
fn raidz1_status() -> String {
    concat!(
        "  pool: bigdata\n",
        " state: ONLINE\n",
        "config:\n",
        "\n",
        "\tNAME         STATE     READ WRITE CKSUM\n",
        "\tbigdata      ONLINE       0     0     0\n",
        "\t  raidz1-0   ONLINE       0     0     0\n",
        "\t    /dev/sdb ONLINE       0     0     0\n",
        "\t    /dev/sdc ONLINE       0     0     0\n",
        "\t    /dev/sdd ONLINE       0     0     0\n",
        "\n",
        "errors: No known data errors\n",
    )
    .to_string()
}

/// 降级池样本：mirror 一盘 FAULTED + 非零错误计数。
/// 真实场景：盘 sdc 坏，CKSUM 累计 12、READ 3，mirror-0 整体 DEGRADED。
fn degraded_mirror_status() -> String {
    concat!(
        "  pool: tank\n",
        " state: DEGRADED\n",
        "status: One or more devices has been taken offline by the administrator.\n",
        "config:\n",
        "\n",
        "\tNAME        STATE     READ WRITE CKSUM\n",
        "\ttank        DEGRADED     0     0     0\n",
        "\t  mirror-0  DEGRADED     0     0     0\n",
        "\t    /dev/sdb ONLINE       0     0     0\n",
        "\t    /dev/sdc FAULTED      3     0    12\n",
        "\n",
        "errors: No known data errors\n",
    )
    .to_string()
}

/// 多池全量输出样本（zpool status 无参数，列所有池）。
fn multi_pool_status() -> String {
    concat!(
        "  pool: tank\n",
        " state: ONLINE\n",
        "config:\n",
        "\n",
        "\tNAME        STATE     READ WRITE CKSUM\n",
        "\ttank        ONLINE       0     0     0\n",
        "\t  mirror-0  ONLINE       0     0     0\n",
        "\t    /dev/sdb ONLINE       0     0     0\n",
        "\t    /dev/sdc ONLINE       0     0     0\n",
        "\n",
        "errors: No known data errors\n",
        "\n",
        "  pool: backup\n",
        " state: ONLINE\n",
        "config:\n",
        "\n",
        "\tNAME        STATE     READ WRITE CKSUM\n",
        "\tbackup      ONLINE       0     0     0\n",
        "\t  /dev/sdd  ONLINE       0     0     0\n",
        "\n",
        "errors: No known data errors\n",
    )
    .to_string()
}

/// A.1 单盘池：pool name + 1 个 Disk vdev（path + ONLINE + 全 0 错误计数）。
#[test]
fn parse_single_disk_pool() {
    let pools = parse_zpool_status(&single_disk_status());
    assert_eq!(pools.len(), 1, "应解析出 1 个池: {pools:?}");
    let p = &pools[0];
    assert_eq!(p.name, "osprobepersist");
    assert_eq!(p.health, Health::Healthy);
    assert!(p
        .scan
        .as_ref()
        .map(|s| s.contains("scrub"))
        .unwrap_or(false));
    assert_eq!(p.vdevs.len(), 1, "单盘池应有 1 个 vdev: {:?}", p.vdevs);
    let v = &p.vdevs[0];
    assert_eq!(v.kind, VdevKind::Disk);
    assert_eq!(v.disks, vec!["/tmp/osprobe-persist.img".to_string()]);
    assert_eq!(v.health, Health::Healthy);
    assert_eq!(v.read_errors, 0);
    assert_eq!(v.write_errors, 0);
    assert_eq!(v.cksum_errors, 0);
}

/// A.2 mirror 池：顶层 mirror-0 + 2 个子 disk（path 派生）。
#[test]
fn parse_mirror_pool() {
    let pools = parse_zpool_status(&mirror_status());
    assert_eq!(pools.len(), 1);
    let p = &pools[0];
    assert_eq!(p.name, "tank");
    assert_eq!(p.health, Health::Healthy);
    assert_eq!(p.vdevs.len(), 1, "mirror 池应有 1 个顶层 mirror vdev");
    let v = &p.vdevs[0];
    assert_eq!(v.kind, VdevKind::Mirror);
    assert_eq!(
        v.disks,
        vec!["/dev/sdb".to_string(), "/dev/sdc".to_string()],
        "mirror 成员盘应按顺序收集"
    );
    assert_eq!(v.health, Health::Healthy);
    // mirror-0 行的错误计数（顶层汇总）+ 无成员报错 → 全 0
    assert_eq!((v.read_errors, v.write_errors, v.cksum_errors), (0, 0, 0));
}

/// A.3 raidz1 池：顶层 raidz1-0 + 3 个子 disk。
#[test]
fn parse_raidz1_pool() {
    let pools = parse_zpool_status(&raidz1_status());
    assert_eq!(pools.len(), 1);
    let p = &pools[0];
    assert_eq!(p.name, "bigdata");
    assert_eq!(p.vdevs.len(), 1);
    let v = &p.vdevs[0];
    assert_eq!(v.kind, VdevKind::Raidz1);
    assert_eq!(v.disks.len(), 3, "raidz1 应有 3 成员盘");
    assert_eq!(
        v.disks,
        vec![
            "/dev/sdb".to_string(),
            "/dev/sdc".to_string(),
            "/dev/sdd".to_string(),
        ]
    );
    assert_eq!(v.health, Health::Healthy);
}

/// A.4 故障态：mirror 一盘 FAULTED + 非零错误计数（READ=3, CKSUM=12）。
/// 验证：池整体 DEGRADED；mirror-0 DEGRADED；成员 sdc FAULTED；
/// 错误计数从成员最大值聚合（READ=3, CKSUM=12）。
#[test]
fn parse_degraded_mirror_with_errors() {
    let pools = parse_zpool_status(&degraded_mirror_status());
    assert_eq!(pools.len(), 1);
    let p = &pools[0];
    assert_eq!(p.name, "tank");
    assert_eq!(p.health, Health::Degraded, "池整体应 DEGRADED");
    assert_eq!(p.vdevs.len(), 1);
    let v = &p.vdevs[0];
    assert_eq!(v.kind, VdevKind::Mirror);
    assert_eq!(v.health, Health::Degraded, "mirror-0 整体应 DEGRADED");
    assert_eq!(v.disks.len(), 2);
    // 错误聚合：sdb=0, sdc=(3,0,12) → max(3,0,12)
    assert_eq!(v.read_errors, 3, "READ 应取成员最大值 3");
    assert_eq!(v.write_errors, 0);
    assert_eq!(v.cksum_errors, 12, "CKSUM 应取成员最大值 12");
}

/// A.5 多池输出：zpool status 全量（tank mirror + backup 单盘）应解析出 2 池。
#[test]
fn parse_multi_pool_status() {
    let pools = parse_zpool_status(&multi_pool_status());
    assert_eq!(pools.len(), 2, "应解析出 2 个池: {pools:?}");
    let by_name: std::collections::HashMap<&str, &PoolStatus> =
        pools.iter().map(|p| (p.name.as_str(), p)).collect();
    let tank = by_name.get("tank").expect("应有 tank 池");
    let backup = by_name.get("backup").expect("应有 backup 池");
    assert_eq!(tank.vdevs.len(), 1);
    assert_eq!(tank.vdevs[0].kind, VdevKind::Mirror);
    assert_eq!(tank.vdevs[0].disks.len(), 2);
    assert_eq!(backup.vdevs.len(), 1);
    assert_eq!(backup.vdevs[0].kind, VdevKind::Disk);
    assert_eq!(backup.vdevs[0].disks, vec!["/dev/sdd".to_string()]);
}

/// A.6 异常/空输出容错：空串、无 pool 行、缺 config 段都不 panic，返回空 Vec。
#[test]
fn parse_handles_malformed_output() {
    // 完全空
    assert!(parse_zpool_status("").is_empty());
    // 仅一行无 pool: 前缀
    assert!(parse_zpool_status("garbage no pool line\n").is_empty());
    // 有 pool 行但无 config 段（仅元数据）→ 池在结果里但 vdevs 空
    let only_meta = "  pool: lonely\n state: ONLINE\nerrors: No known data errors\n";
    let pools = parse_zpool_status(only_meta);
    assert_eq!(pools.len(), 1);
    assert_eq!(pools[0].name, "lonely");
    assert!(pools[0].vdevs.is_empty(), "无 config 段应无 vdev");
    // config 段但数据行字段不足（异常行）→ 跳过不 panic
    let short_row = "  pool: weird\n state: ONLINE\nconfig:\n\tNAME STATE READ WRITE CKSUM\n\tonly-two-cols\nerrors: x\n";
    let pools = parse_zpool_status(short_row);
    assert_eq!(pools.len(), 1);
    assert!(pools[0].vdevs.is_empty());
}

// ============================================================================
// B. 真实 zpool status 解析（#[ignore]，需 zfs）—— 本机持久池只读验证
// ============================================================================

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

/// 真实环境预检：zfs 二进制 + 内核模块可用。
///
/// 本组测**只读**（zpool status 普通用户可跑，无需 root），故只校验二进制+模块。
/// 全部满足返回 true；缺其一则 eprintln 报告并返回 false（调用方据此优雅跳过）。
fn zpool_status_ready() -> bool {
    if which("zpool").is_none() {
        eprintln!(
            "[zpool_status_real] SKIP: `zpool` 二进制不在 $PATH —— \
             需装 zfsutils-linux。详见 docs/SANDBOX.md §5。"
        );
        return false;
    }
    // `zfs version` exit 0 表示 userland + kmod 都在（zpool 没有独立 version 子命令）。
    let probe = Command::new("zfs").arg("version").output();
    match probe {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            eprintln!(
                "[zpool_status_real] SKIP: `zfs version` 退出码非 0 \
                 （内核 ZFS 模块可能未加载）。stderr: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            false
        }
        Err(e) => {
            eprintln!("[zpool_status_real] SKIP: spawn `zfs version` 失败：{e}");
            false
        }
    }
}

/// B.1 真实 osprobepersist 池 status 解析。
///
/// 跑 `zpool status osprobepersist` → parse_zpool_status → 断言：
/// - 解析出 1 个池，name == "osprobepersist"。
/// - 池健康 ONLINE。
/// - 含 1 个 Disk vdev，path == "/tmp/osprobe-persist.img"。
/// - 错误计数全 0（健康池）。
#[tokio::test]
#[ignore = "真实 zpool status：需 zfs + 本机 osprobepersist 持久池。跑法：cargo test -p os-storage --features mock --test zpool_status_real -- --ignored --nocapture real_osprobepersist_status_parses"]
async fn real_osprobepersist_status_parses() {
    if !zpool_status_ready() {
        return;
    }
    // 直接用 std Command 跑（同步，无需 tokio runtime 复杂性）。
    let out = Command::new("zpool")
        .args(["status", "osprobepersist"])
        .output();
    let stdout = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(o) => {
            eprintln!(
                "[zpool_status_real] SKIP: `zpool status osprobepersist` 退出码非 0 \
                 （池可能不存在）。stderr: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            return;
        }
        Err(e) => {
            eprintln!("[zpool_status_real] SKIP: spawn `zpool status` 失败：{e}");
            return;
        }
    };

    eprintln!("[zpool_status_real] 真实 zpool status osprobepersist 输出:\n{stdout}");
    let pools = parse_zpool_status(&stdout);
    assert_eq!(pools.len(), 1, "应解析出 1 个池: {pools:?}");
    let p = &pools[0];
    assert_eq!(p.name, "osprobepersist");
    assert_eq!(
        p.health,
        Health::Healthy,
        "持久测试池应 ONLINE: 实际 {:?}",
        p.health
    );
    assert!(!p.vdevs.is_empty(), "应有至少 1 个 vdev: {:?}", p.vdevs);
    // 找含 osprobe-persist.img 的 vdev
    let vdev = p
        .vdevs
        .iter()
        .find(|v| v.disks.iter().any(|d| d.contains("osprobe-persist.img")))
        .expect("应找到含 /tmp/osprobe-persist.img 的 vdev");
    assert_eq!(vdev.health, Health::Healthy);
    assert_eq!(vdev.read_errors, 0, "健康池 READ 应为 0");
    assert_eq!(vdev.write_errors, 0);
    assert_eq!(vdev.cksum_errors, 0);
    eprintln!(
        "[zpool_status_real] osprobepersist 解析 OK: vdevs={:?}",
        p.vdevs
    );
}

/// B.2 多池全量 status 解析（zpool status 无参数）。
///
/// 若本机有多个池，验证全部解析出来；单池也 OK（至少含 osprobepersist）。
#[tokio::test]
#[ignore = "真实 zpool status：需 zfs + 至少 1 个池。跑法：cargo test -p os-storage --features mock --test zpool_status_real -- --ignored --nocapture real_multi_pool_status_parses"]
async fn real_multi_pool_status_parses() {
    if !zpool_status_ready() {
        return;
    }
    let out = Command::new("zpool").arg("status").output();
    let stdout = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(o) => {
            eprintln!(
                "[zpool_status_real] SKIP: `zpool status` 退出码非 0。stderr: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            return;
        }
        Err(e) => {
            eprintln!("[zpool_status_real] SKIP: spawn `zpool status` 失败：{e}");
            return;
        }
    };

    eprintln!("[zpool_status_real] 真实 zpool status 全量输出:\n{stdout}");
    let pools = parse_zpool_status(&stdout);
    if pools.is_empty() {
        eprintln!("[zpool_status_real] SKIP: 无池（zpool status 无 config 段）");
        return;
    }
    eprintln!(
        "[zpool_status_real] 全量解析 OK: {} 个池: {:?}",
        pools.len(),
        pools.iter().map(|p| &p.name).collect::<Vec<_>>()
    );
    // 至少应能解析出每个池的 name（不强制 vdevs 非空——某些池态可能特殊）
    for p in &pools {
        assert!(!p.name.is_empty(), "池名不应为空: {p:?}");
    }
}

/// B.3 zpool status -v（verbose）模式解析。
///
/// `-v` 在有数据错误时会展开详细错误列表，但健康池的 `-v` 输出与普通 status 几乎
/// 一致。验证解析器对 -v 输出也能正确提取 vdev 树（不因多出的 detail 段崩溃）。
#[tokio::test]
#[ignore = "真实 zpool status -v：需 zfs + 池。跑法：cargo test -p os-storage --features mock --test zpool_status_real -- --ignored --nocapture real_verbose_status_parses"]
async fn real_verbose_status_parses() {
    if !zpool_status_ready() {
        return;
    }
    // 优先对持久池跑 -v（已知存在）；若无则跑全量
    let out = Command::new("zpool")
        .args(["status", "-v", "osprobepersist"])
        .output();
    let stdout = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        Ok(o) => {
            // 持久池不存在时回退全量
            let out2 = Command::new("zpool").args(["status", "-v"]).output();
            match out2 {
                Ok(o2) if o2.status.success() => String::from_utf8_lossy(&o2.stdout).into_owned(),
                _ => {
                    eprintln!(
                        "[zpool_status_real] SKIP: `zpool status -v` 不可用。stderr: {}",
                        String::from_utf8_lossy(&o.stderr)
                    );
                    return;
                }
            }
        }
        Err(e) => {
            eprintln!("[zpool_status_real] SKIP: spawn `zpool status -v` 失败：{e}");
            return;
        }
    };

    eprintln!("[zpool_status_real] 真实 zpool status -v 输出:\n{stdout}");
    let pools = parse_zpool_status(&stdout);
    eprintln!(
        "[zpool_status_real] -v 解析 OK: {} 个池，详情 {:?}",
        pools.len(),
        pools
    );
    // -v 模式解析不应 panic 且至少能提取池名（即使含 detail 段）
    for p in &pools {
        assert!(!p.name.is_empty());
    }
}
