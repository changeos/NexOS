//! osd `CgroupsRsBackend` 真实 cgroup v2 写入实跑验证（`#[ignore]`，需 root + cgroup2fs）。
//!
//! 对应 docs/SANDBOX.md §5「应入沙箱测试清单」的 cgroup 项，及
//! `crates/osd/src/impl_orchestrator.rs:341` `set_quota` 经 `CgroupQuota` →
//! `CgroupsRsBackend`（生产默认后端）写 `/sys/fs/cgroup/<base>/<id>` 的
//! `cpu.max` / `memory.max` 的真实路径。逻辑此前已接通 cgroups-rs 但**从未在本机
//! root + cgroup2fs 环境真跑验证**——本测补上这一环。
//!
//! ## 验证内容
//! 1. **apply_quota 真实写**（`real_cgroup_apply_writes_cpu_max_and_memory_max`）：
//!    构造 `CgroupsRsBackend`，apply 一个组件的 cpu=0.5核/mem=100MB，断言
//!    `/sys/fs/cgroup/<base>/<id>/cpu.max` 与 `memory.max` 真实存在且内容正确。
//! 2. **read_quota 真实读回**（`real_cgroup_read_returns_what_was_written`）：
//!    apply 后 read_quota 读回，断言与写入值一致（CPU 核数 / 内存字节往返）。
//! 3. **apply_quota 真实更新**（`real_cgroup_apply_updates_in_place`）：
//!    apply cpu=0.5 再 apply cpu=2.0，断言 `cpu.max` 文件内容真更新为新值。
//! 4. **teardown 真实清理**（`real_cgroup_teardown_removes_cgroup_dir`）：
//!    apply 建 cgroup 后用 `Cgroup::delete()` 删，断言目录消失。
//!
//! ## 跑法
//! 需 root（写 `/sys/fs/cgroup` 需 CAP_SYS_ADMIN）+ cgroup v2 unified 挂载。
//! ```bash
//! sudo cargo test -p osd --features mock --test cgroup_real -- --ignored --nocapture
//! ```
//! 非 root / 非 cgroup v2：**优雅跳过**（`eprintln` 报告缺什么，不 panic），不污染
//! 默认 `cargo test` 套件（`#[ignore]` 默认不执行）。
//!
//! ## 红线
//! **严禁**写 `/sys/fs/cgroup/os/` 生产路径——base 用唯一测试前缀
//! `osd_test_<pid>_<ts>`，4 测共享一个 base 目录；RAII guard 保证即使断言失败也
//! `remove_dir` 拆掉 cgroup（Drop 用同步 `std::fs::remove_dir`，不在 tokio::test
//! runtime 里建嵌套 runtime——与 `crates/os-storage/tests/real_zfs_ops.rs`
//! `RealPoolGuard::drop` 同源策略，batch3 zfs-real 踩过该坑）。

#![cfg(feature = "mock")] // 真实 cgroup 测在 mock feature 下编译（与 real_zfs_ops 同约定）

use std::path::Path;
use std::process::Command;

use os_core::ResourceQuota;
use osd::{CgroupBackend, CgroupsRsBackend, ComponentId};

/// `/sys/fs/cgroup` 挂载点根（cgroup v2 unified）。
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// 生成唯一测试 base 名（带 PID + 纳秒时间戳，防并发测冲突且绝不与生产 `os` 撞）。
///
/// 前缀 `osd_test_` 明确标识为测试用途；若清理失败残留，运维一眼可辨。
fn unique_base() -> String {
    format!(
        "osd_test_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

/// 真实环境预检：cgroup v2 unified 挂载 + root。
///
/// 全部满足返回 `(base, CgroupsRsBackend)` 三元组，可直接开始真写；缺其一则
/// `eprintln` 报告缺什么并返回 `None`（调用方优雅跳过，不 panic）。
fn real_env_ready() -> Option<(String, CgroupsRsBackend)> {
    // 1. cgroup v2 unified 挂载检查：stat -fc %T /sys/fs/cgroup 返回 cgroup2fs。
    //    用 `stat -fc %T` 子进程（最贴近规格书给出的检测命令）。
    let stat = Command::new("stat")
        .args(["-fc", "%T", CGROUP_ROOT])
        .output();
    let is_v2 = match stat {
        Ok(o) if o.status.success() => {
            let fstype = String::from_utf8_lossy(&o.stdout).trim().to_string();
            fstype == "cgroup2fs"
        }
        other => {
            eprintln!(
                "[cgroup_real] SKIP: `stat -fc %T {CGROUP_ROOT}` 失败: {other:?} \
                 —— 无法确认 cgroup v2 挂载。"
            );
            return None;
        }
    };
    if !is_v2 {
        eprintln!(
            "[cgroup_real] SKIP: {CGROUP_ROOT} 非 cgroup2fs —— 本测仅支持 cgroup v2 \
             unified 模式。"
        );
        return None;
    }

    // 2. root 检查（写 /sys/fs/cgroup 需 CAP_SYS_ADMIN）。
    let uid = Command::new("id").arg("-u").output();
    let is_root = matches!(
        uid,
        Ok(o) if String::from_utf8_lossy(&o.stdout).trim() == "0"
    );
    if !is_root {
        eprintln!(
            "[cgroup_real] SKIP: 非 root（写 {CGROUP_ROOT} 需 root + CAP_SYS_ADMIN）。\
             跑法：sudo cargo test -p osd --features mock --test cgroup_real -- --ignored"
        );
        return None;
    }

    Some((unique_base(), CgroupsRsBackend::new()))
}

/// RAII 拆 cgroup（即使断言失败也清理）。
///
/// Drop 用**同步** `std::fs::remove_dir`（不经 tokio runtime，也不 spawn 子进程）：
/// 本测在 `sudo cargo test` 下整进程 euid=0，直接同步删目录最简且零嵌套 runtime 风险
/// —— 与 `crates/os-storage/tests/real_zfs_ops.rs::RealPoolGuard::drop` 同源策略
/// （batch3 zfs-real 踩过：RAII Drop 在 tokio::test 线程内 block_on 建嵌套 runtime 会
/// panic "Cannot start a runtime from within a runtime"）。cgroup v2 删目录有顺序要求：
/// 必须先删子 cgroup（`base/id`）再删父（`base`），否则 `remove_dir` 因目录非空失败。
/// 任一步失败静默忽略——teardown 是「尽力清理」。
struct CgroupGuard {
    /// /sys/fs/cgroup/<base>（父目录，最后删）
    base_dir: String,
    /// /sys/fs/cgroup/<base>/<id>（子 cgroup，先删）
    id_dirs: Vec<String>,
}

impl CgroupGuard {
    /// 登记一个子 cgroup 路径（apply 建好后调用，供 Drop 清理）。
    fn track(&mut self, id: &str) {
        let p = format!("{}/{}", self.base_dir, id);
        self.id_dirs.push(p);
    }
}

impl Drop for CgroupGuard {
    fn drop(&mut self) {
        // 先删子 cgroup（cgroup v2：非空目录 remove_dir 会失败，故由内向外删）。
        for id_dir in &self.id_dirs {
            match std::fs::remove_dir(id_dir) {
                Ok(()) => eprintln!("[cgroup_real] 清理：已删 cgroup {id_dir}"),
                Err(e) => {
                    eprintln!("[cgroup_real] 清理失败（cgroup 可能已删或残留进程）：{id_dir}: {e}")
                }
            }
        }
        // 再删 base 父目录。
        match std::fs::remove_dir(&self.base_dir) {
            Ok(()) => eprintln!("[cgroup_real] 清理：已删 base 目录 {}", self.base_dir),
            Err(e) => eprintln!(
                "[cgroup_real] 清理失败（base 目录可能仍有子项未删）：{}: {e}",
                self.base_dir
            ),
        }
    }
}

/// 读 cgroup 文件首行（如 cpu.max / memory.max），返回去尾换行后的内容。
fn read_cgroup_file(base: &str, id: &str, file: &str) -> String {
    let path = format!("{CGROUP_ROOT}/{base}/{id}/{file}");
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("[cgroup_real] 读 {path} 失败: {e}"));
    content.trim_end().to_string()
}

/// 把 cgroup v2 文件读回的内存上限字符串（如 "99999744" 或 "max"）解析为 `Option<u64>`。
/// "max"（不限）→ None；数字 → Some(bytes)。
fn parse_mem_max(s: &str) -> Option<u64> {
    if s.trim() == "max" {
        None
    } else {
        Some(
            s.trim()
                .parse::<u64>()
                .unwrap_or_else(|e| panic!("[cgroup_real] 解析 memory.max 失败: {s:?}: {e}")),
        )
    }
}

/// **真实跑发现（非 osd bug）**：cgroup v2 内核对 `memory.max` 做 **PAGE_SIZE 对齐**，
/// 写 `100_000_000` 字节会被内核向下取整到页边界：
///   `floor(100_000_000 / 4096) * 4096 = 99_999_744`（差 256 字节）。
///
/// 这是 cgroup v2 文档化的内核行为（见 `Documentation/admin-guide/cgroup-v2.rst`
/// memory.max 条目：「the actual limit may be rounded down to be a multiple of
/// the system page size」），**不是** `CgroupsRsBackend` 实现的 bug——写入路径
/// 与读回路径都正确，只是内核存储的是页对齐值。
///
/// 本辅助断言「读回值等于 request 向下取整到 PAGE_SIZE（即内核实际存的值）」，
/// 并保证 `request` 自身不丢任何字节上限语义（读回值 ≤ request 且差 < 1 page）。
const PAGE_SIZE: u64 = 4096;

fn assert_mem_max_kernel_aligned(actual: Option<u64>, requested: Option<u64>, label: &str) {
    match (actual, requested) {
        (Some(a), Some(req)) => {
            let aligned = (req / PAGE_SIZE) * PAGE_SIZE; // 内核页对齐后的值
            assert_eq!(
                a, aligned,
                "{label}：memory.max 应为 request 页对齐值 {aligned}（内核 floor 到 PAGE_SIZE），实际 {a}（request={req}）"
            );
            // 上限语义不丢：读回值 ≤ request 且差不超 1 页。
            assert!(
                a <= req && req - a < PAGE_SIZE,
                "{label}：读回内存上限 {a} 应 ≤ request {req} 且差 < 1 页"
            );
        }
        (None, None) => {} // 都是 max（不限），一致
        (actual, requested) => panic!(
            "[cgroup_real] {label}：memory.max 上限/不限状态不一致：实际={actual:?} 期望={requested:?}"
        ),
    }
}

// ---------------------------------------------------------------------------
// 测 a：apply_quota 真实写 cpu.max / memory.max
// ---------------------------------------------------------------------------

/// 真实写：`CgroupsRsBackend::apply_quota` 写 cpu=0.5核(50000us/100000) + mem=100MB，
/// 断言 `/sys/fs/cgroup/<base>/<id>/cpu.max` 与 `memory.max` 真实存在且内容正确。
#[test]
#[ignore = "真实 cgroup v2 写入：需 root + cgroup2fs。跑法：sudo cargo test -p osd --features mock --test cgroup_real -- --ignored --nocapture"]
fn real_cgroup_apply_writes_cpu_max_and_memory_max() {
    let (base, backend) = match real_env_ready() {
        Some(v) => v,
        None => return,
    };

    let mut guard = CgroupGuard {
        base_dir: format!("{CGROUP_ROOT}/{base}"),
        id_dirs: vec![],
    };

    let id = ComponentId::new("comp-a");
    // cpu=0.5核 → CFS quota = 0.5 * 100000 = 50000us，period=100000
    // mem=100_000_000 字节
    let quota = ResourceQuota {
        cpu_cores: Some(0.5),
        memory_bytes: Some(100_000_000),
        io_bps_limit: None,
    };

    eprintln!("[cgroup_real] apply_quota base={base} id=comp-a cpu=0.5核 mem=100MB");
    backend
        .apply_quota(&base, &id, &quota)
        .expect("apply_quota 在 root + cgroup2fs 应成功");
    guard.track("comp-a");

    // 断言 1：cgroup 目录与 cpu.max / memory.max 文件真实存在。
    let cpu_max_path = format!("{CGROUP_ROOT}/{base}/comp-a/cpu.max");
    let mem_max_path = format!("{CGROUP_ROOT}/{base}/comp-a/memory.max");
    assert!(
        Path::new(&cpu_max_path).exists(),
        "cpu.max 应被真实创建：{cpu_max_path}"
    );
    assert!(
        Path::new(&mem_max_path).exists(),
        "memory.max 应被真实创建：{mem_max_path}"
    );

    // 断言 2：内容正确。cpu.max 形如 "50000 100000"，memory.max 形如 "100000000"。
    let cpu_max = read_cgroup_file(&base, "comp-a", "cpu.max");
    let mem_max = read_cgroup_file(&base, "comp-a", "memory.max");
    eprintln!("[cgroup_real] 真读回 cpu.max={cpu_max:?} memory.max={mem_max:?}");

    assert_eq!(
        cpu_max, "50000 100000",
        "cpu.max 应为 '50000 100000'（0.5核 × 100000us / 周期 100000us）"
    );
    // 真实跑发现：cgroup v2 内核对 memory.max 做 PAGE_SIZE 对齐（见
    // assert_mem_max_kernel_aligned 注释），写 100_000_000 实际存 99_999_744。
    // 这不是 osd 实现 bug，是内核行为；测用页对齐断言容忍之。
    assert_mem_max_kernel_aligned(parse_mem_max(&mem_max), Some(100_000_000), "测 a apply");

    eprintln!("[cgroup_real] 测 a 通过：apply_quota 真实写入 cpu.max + memory.max 内容正确（memory.max 经内核 PAGE 对齐）");
    // guard Drop 自动清理（即使上面 assert 失败）
}

// ---------------------------------------------------------------------------
// 测 b：read_quota 真实读回写入值
// ---------------------------------------------------------------------------

/// 真实读回：apply cpu=0.5/mem=100MB 后 `read_quota` 读回，断言值一致。
#[test]
#[ignore = "真实 cgroup v2 读回：需 root + cgroup2fs。跑法：sudo cargo test -p osd --features mock --test cgroup_real -- --ignored --nocapture"]
fn real_cgroup_read_returns_what_was_written() {
    let (base, backend) = match real_env_ready() {
        Some(v) => v,
        None => return,
    };

    let mut guard = CgroupGuard {
        base_dir: format!("{CGROUP_ROOT}/{base}"),
        id_dirs: vec![],
    };

    let id = ComponentId::new("comp-b");
    let quota = ResourceQuota {
        cpu_cores: Some(0.5),
        memory_bytes: Some(100_000_000),
        io_bps_limit: None,
    };

    backend
        .apply_quota(&base, &id, &quota)
        .expect("apply_quota 应成功");
    guard.track("comp-b");

    let read_back = backend
        .read_quota(&base, &id)
        .expect("read_quota 不应返回 Err")
        .expect("read_quota 在 cgroup 存在时应返回 Some");

    eprintln!(
        "[cgroup_real] read_quota 读回 cpu_cores={:?} memory_bytes={:?}",
        read_back.cpu_cores, read_back.memory_bytes
    );
    // CPU 核数：0.5 核往返（50000us / 100000us = 0.5）
    assert_eq!(read_back.cpu_cores, Some(0.5), "CPU 核数应读回 0.5");
    // 内存字节：100_000_000 经内核 PAGE 对齐后为 99_999_744（非 osd bug，见
    // assert_mem_max_kernel_aligned 注释），测用页对齐断言容忍。
    assert_mem_max_kernel_aligned(read_back.memory_bytes, Some(100_000_000), "测 b read_quota");

    eprintln!(
        "[cgroup_real] 测 b 通过：read_quota 读回与 apply 写入一致（memory 经内核 PAGE 对齐）"
    );
}

// ---------------------------------------------------------------------------
// 测 c：apply_quota 真实更新（同一 cgroup 改值）
// ---------------------------------------------------------------------------

/// 真实更新：apply cpu=0.5 再 apply cpu=2.0，断言 cpu.max 文件内容真更新为新值。
#[test]
#[ignore = "真实 cgroup v2 更新：需 root + cgroup2fs。跑法：sudo cargo test -p osd --features mock --test cgroup_real -- --ignored --nocapture"]
fn real_cgroup_apply_updates_in_place() {
    let (base, backend) = match real_env_ready() {
        Some(v) => v,
        None => return,
    };

    let mut guard = CgroupGuard {
        base_dir: format!("{CGROUP_ROOT}/{base}"),
        id_dirs: vec![],
    };

    let id = ComponentId::new("comp-c");

    // 第一次：cpu=0.5核 → "50000 100000"
    let q1 = ResourceQuota {
        cpu_cores: Some(0.5),
        memory_bytes: Some(100_000_000),
        io_bps_limit: None,
    };
    backend
        .apply_quota(&base, &id, &q1)
        .expect("apply #1 应成功");
    guard.track("comp-c");
    let cpu_max_1 = read_cgroup_file(&base, "comp-c", "cpu.max");
    assert_eq!(
        cpu_max_1, "50000 100000",
        "首次 apply 后 cpu.max 应为 0.5 核"
    );

    // 第二次：cpu=2.0核 → "200000 100000"
    let q2 = ResourceQuota {
        cpu_cores: Some(2.0),
        memory_bytes: Some(100_000_000),
        io_bps_limit: None,
    };
    backend
        .apply_quota(&base, &id, &q2)
        .expect("apply #2 应成功");
    let cpu_max_2 = read_cgroup_file(&base, "comp-c", "cpu.max");
    eprintln!("[cgroup_real] 更新后 cpu.max: {cpu_max_1:?} → {cpu_max_2:?}");
    assert_eq!(
        cpu_max_2, "200000 100000",
        "第二次 apply 后 cpu.max 应更新为 2.0 核（200000us）"
    );

    eprintln!("[cgroup_real] 测 c 通过：apply_quota 真实更新 cpu.max 内容");
}

// ---------------------------------------------------------------------------
// 测 d：teardown 真实清理（Cgroup::delete 删目录）
// ---------------------------------------------------------------------------

/// 真实清理：apply 建 cgroup 后用 `cgroups_rs::fs::Cgroup::delete()` 删，
/// 断言 `/sys/fs/cgroup/<base>/<id>` 目录消失。
#[test]
#[ignore = "真实 cgroup v2 清理：需 root + cgroup2fs。跑法：sudo cargo test -p osd --features mock --test cgroup_real -- --ignored --nocapture"]
fn real_cgroup_teardown_removes_cgroup_dir() {
    let (base, backend) = match real_env_ready() {
        Some(v) => v,
        None => return,
    };

    // base 目录（guard 接管：删 id 子目录后删 base）
    let mut guard = CgroupGuard {
        base_dir: format!("{CGROUP_ROOT}/{base}"),
        id_dirs: vec![],
    };

    let id = ComponentId::new("comp-d");
    let quota = ResourceQuota {
        cpu_cores: Some(0.5),
        memory_bytes: Some(100_000_000),
        io_bps_limit: None,
    };
    backend
        .apply_quota(&base, &id, &quota)
        .expect("apply 应成功");
    guard.track("comp-d");

    let id_dir = format!("{CGROUP_ROOT}/{base}/comp-d");
    assert!(
        Path::new(&id_dir).exists(),
        "apply 后 cgroup 目录应存在：{id_dir}"
    );

    // 用 cgroups-rs Cgroup::delete() 真实删（与生产路径同源：删除 = remove_dir on v2）。
    let hier = Box::new(cgroups_rs::fs::hierarchies::V2::new());
    let cg = cgroups_rs::fs::Cgroup::load(hier, format!("{base}/comp-d"));
    cg.delete().expect("Cgroup::delete 在空 cgroup 上应成功");

    // 断言：id 子目录已消失（base 目录还在，由 guard 收尾删）。
    assert!(
        !Path::new(&id_dir).exists(),
        "delete 后 cgroup 目录应消失：{id_dir}"
    );
    // guard 里 id_dir 已被手动删，从 guard 移除避免 Drop 重复删报错。
    guard.id_dirs.clear();

    eprintln!("[cgroup_real] 测 d 通过：Cgroup::delete() 真实删除 cgroup 目录");
    // guard Drop 删 base 目录
}
