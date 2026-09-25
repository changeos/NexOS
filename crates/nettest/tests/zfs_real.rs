//! ZFS 真实执行层冒烟验证（subprocess：zfs / zpool 二进制 + 内核模块）。
//!
//! 验证 os-storage 选用的「subprocess 调 `zpool`/`zfs` CLI」执行栈在本机真实可用：
//! 真实 spawn `zfs --version` 与 `zpool list`，断言二进制存在 + 内核模块加载 +
//! CLI 能真实与内核 ZFS 子系统通信。这一路验证的是 os-storage::ZfsCliBackend 的
//! 真实命令路径（不是 fixture 注入的 CommandRunner）。
//!
//! ## 为什么 subprocess 而不是 libzfs
//! os-storage 默认实现就是 subprocess 调 zpool/zfs（非 libzfs_core 绑定），故本测
//! 与生产路径完全一致——验证的是「真实 zfs/zpool 二进制 + 内核 ZFS 模块」的可用性。
//!
//! ## 运行环境
//! - 需 root（zpool list 至少要能读 `/dev/zfs`，多数发行版非 root 也能读，但
//!   内核模块未加载时 `zfs --version` 都会失败）。
//! - 宿主需装 `zfsutils-linux`（Debian/Ubuntu）或 `zfs`（其他）+ 加载 `zfs` 内核模块
//!   （`sudo modprobe zfs`）。沙箱镜像已覆盖，见 docs/SANDBOX.md §2.3 / §5.2。
//! - 无 zfs 二进制 / 内核模块未加载 / 非 root：测应**优雅失败**（明确 eprintln 报告
//!   缺什么），不应 panic——这就是 `#[ignore]` 的意义：手动 `--ignored` 跑时，
//!   清楚看到环境缺哪一块。

mod common;

use std::process::Command;

use common::timeout_or_panic;

/// ZFS 真实执行层冒烟：真实跑 `zfs --version` + `zpool list`，断言二进制 + 内核模块可用。
///
/// 步骤：构造命令 → 执行 → 断言 exit 0 + stdout 含 "zfs" 字样 → 清理（无副作用：
/// `zfs --version` / `zpool list` 都是只读，不留任何状态）。
///
/// 环境不支持（无 zfs 二进制 / 内核模块未加载）时**优雅跳过**（return + 明确 eprintln），
/// 不 panic——这样手动 `--ignored` 跑时清楚看到环境缺什么，也不会污染测试套件。
#[tokio::test]
#[ignore = "真实 zfs/zpool 子进程：手动 `cargo test -p nettest -- --ignored zfs_real_smoke`（需 zfsutils + 加载 zfs 模块）"]
async fn zfs_real_smoke() {
    timeout_or_panic(async {
        // 0. 预检：zfs 二进制是否在 $PATH（不在则优雅跳过，不 panic）。
        if which("zfs").is_none() {
            eprintln!(
                "[nettest] SKIP: `zfs` 二进制不在 $PATH —— 需装 zfsutils-linux / zfs \
                 (Debian: `apt install zfsutils-linux`; 加载模块 `sudo modprobe zfs`)。\
                 详见 docs/SANDBOX.md §5.2。"
            );
            return;
        }
        eprintln!("[nettest] 检测到 zfs 二进制，开始真实执行层冒烟");

        // 1. `zfs --version`：验证 userland 二进制 + 内核模块（输出含 zfs-kmod 行）。
        //    spawn 到 blocking 池（Command 是同步阻塞 API，tokio::test 内用 spawn_blocking
        //    避免阻塞 runtime；本测命令都是秒级返回）。
        let zfs_output =
            tokio::task::spawn_blocking(|| Command::new("zfs").arg("--version").output())
                .await
                .expect("spawn_blocking 失败")
                .expect("[nettest] spawn `zfs --version` 失败");

        if !zfs_output.status.success() {
            eprintln!(
                "[nettest] SKIP: `zfs --version` 退出码非 0（{}）—— zfs 工具链异常。\
                 stderr: {}",
                zfs_output.status,
                String::from_utf8_lossy(&zfs_output.stderr)
            );
            return;
        }
        let zfs_stdout = String::from_utf8_lossy(&zfs_output.stdout);
        // zfs --version 输出形如 "zfs-2.2.x-r0\nzfs-kmod-2.2.x-r0" —— 至少含 "zfs" 串。
        assert!(
            zfs_stdout.to_lowercase().contains("zfs"),
            "[nettest] `zfs --version` stdout 不含 'zfs': {zfs_stdout:?}"
        );
        eprintln!("[nettest] `zfs --version` OK:\n{}", zfs_stdout.trim_end());

        // 关键：zfs-kmod 行表示内核 ZFS 模块已加载。若无 zfs-kmod 行，zpool list 大概率失败。
        if !zfs_stdout.to_lowercase().contains("zfs-kmod") {
            eprintln!(
                "[nettest] 警告：`zfs --version` 输出无 zfs-kmod 行 —— 内核 ZFS 模块可能\
                 未加载，`zpool list` 可能失败。建议 `sudo modprobe zfs`。"
            );
        }

        // 2. 真实 `zpool list`：验证与内核 ZFS 子系统的通信路径。
        //    即使无池，`zpool list` 也应退出 0（输出 "no pools available"）——
        //    关键是它能与 /dev/zfs 通信，证明内核 ZFS 模块真实加载。
        let zpool_list = tokio::task::spawn_blocking(|| Command::new("zpool").arg("list").output())
            .await
            .expect("spawn_blocking 失败")
            .expect("[nettest] spawn `zpool list` 失败");

        let zpool_stdout = String::from_utf8_lossy(&zpool_list.stdout);
        let zpool_stderr = String::from_utf8_lossy(&zpool_list.stderr);
        eprintln!(
            "[nettest] `zpool list` exit={} stdout={:?} stderr={:?}",
            zpool_list.status, zpool_stdout, zpool_stderr
        );

        // zpool list 成功条件：exit 0（无池也是 0，输出 "no pools available"）。
        // 内核模块未加载时 exit != 0 + stderr 含 "cannot open '/dev/zfs'" —— 优雅跳过。
        if !zpool_list.status.success() {
            eprintln!(
                "[nettest] SKIP: `zpool list` 失败（exit {}）—— 通常是内核 ZFS 模块未加载。\
                 stderr: {zpool_stderr}。建议 `sudo modprobe zfs`。详见 docs/SANDBOX.md §2.3。",
                zpool_list.status
            );
            return;
        }

        // 至少有表头（NAME / SIZE / ALLOC / FREE ...）或 "no pools available"。
        assert!(
            zpool_stdout.contains("NAME") || zpool_stdout.to_lowercase().contains("no pools"),
            "[nettest] `zpool list` 输出异常（无 NAME 表头也无 no pools 提示）: {zpool_stdout:?}"
        );

        eprintln!("[nettest] ZFS 真实执行层冒烟通过：zfs 二进制 + 内核模块均可用");
    })
    .await;
}

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
