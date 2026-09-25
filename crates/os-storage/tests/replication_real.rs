//! `ZfsSendRecv` 复制 + `ZfsNativeCrypto` 命令构造验证 + 真实 zfs send-recv 往返测。
//!
//! 对应 docs/SANDBOX.md §5「应入沙箱测试清单」的 zfs send-recv / 加密项。分两类：
//!
//! ## A. 命令构造验证（默认跑，纯逻辑）
//! `ZfsSendRecv::send_cmd`/`recv_cmd` 返回 `tokio::process::Command`，后者无公开 argv
//! 访问器；故实现抽出纯函数 `send_argv`/`recv_argv` 返回 `(program, Vec<String>)`，
//! 本文件对其做断言。crypto 经 `CommandRunner` 注入捕获型 runner 验证 argv。
//!
//! ## B. 真实 zfs send-recv 往返（`#[ignore]`，需 root + zfs）
//! 自包含：建稀疏文件 vdev → 建唯一测试池 → send/recv 往返 → RAII 销毁池 + 删文件。
//! 覆盖：本地管道往返 / send 到文件再 recv / 增量 send-recv / 加密数据集 + passphrase。
//!
//! ## 跑法
//! ```bash
//! # 默认（命令构造测，无需 root）：
//! cargo test -p os-storage --features mock --test replication_real
//!
//! # 真实往返（需 root + zfsutils-linux + zfs 模块）：
//! sudo env PATH=$HOME/.cargo/bin:/usr/bin:/bin RUSTUP_HOME=$HOME/.rustup CARGO_HOME=$HOME/.cargo \
//!   cargo test -p os-storage --features mock --test replication_real -- --ignored --nocapture
//! ```
//! 非 root / 无 zfs / 模块未加载：优雅跳过（eprintln 报缺什么，不 panic）。
//!
//! ## 红线
//! - 唯一 `osprobesr` 前缀 + 独立稀疏 vdev，**绝不碰宿主真实 pool**。
//! - **不**在 LIO export 过的 pool（如 block_real_export 的 `osprobepersist`）上跑
//!   send-recv——batch5 发现 ZFS/LIO 交互会挂起内核线程。本测每个测自建独立池。
//! - RAII guard 保证即使断言失败也 destroy 池（同步 `zpool destroy -f`，避免嵌套
//!   runtime panic）。

#![cfg(feature = "mock")]

use os_core::{DatasetId, SnapshotId};
use os_storage::{CryptoManager, ZfsNativeCrypto, ZfsSendRecv};
use std::process::Command;

// ============================================================================
// 常量 / 助手（与 real_zfs_ops.rs / block_real_export.rs 风格一致）
// ============================================================================

/// 临时池名前缀——避免与真实池（含 block_real_export 的 osprobepersist）冲突。
const POOL_PREFIX: &str = "osprobesr";

/// vdev 稀疏文件大小（1G 足够建数据集做快照 send-recv；sparse 不真占盘）。
const VDEV_SIZE: &str = "1G";

/// 纯 Rust 的 `which`：扫 $PATH 找可执行文件（避免引 which crate）。
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

/// 生成唯一临时池名（带 PID + 纳秒时间戳 + counter，防并发测冲突）。
/// counter 兜底保证同进程多测也不撞名（每个测建独立池）。
fn unique_pool(tag: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    format!(
        "{POOL_PREFIX}_{tag}_{}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
        n,
    )
}

/// 生成唯一临时 vdev 文件路径（与池名配对，同 PID + 纳秒 + counter）。
fn unique_vdev(tag: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "/tmp/{POOL_PREFIX}_{tag}_{}_{nanos}_{n}.img",
        std::process::id(),
    )
}

/// 真实环境预检：zfs 二进制 + 内核模块可用 + root。全满足返回 true；缺其一则
/// eprintln 报告缺什么并返回 false（调用方据此优雅跳过）。
fn real_zfs_ready() -> bool {
    if which("zfs").is_none() {
        eprintln!(
            "[replication_real] SKIP: `zfs` 二进制不在 $PATH —— 需装 zfsutils-linux \
             (Debian: `apt install zfsutils-linux`)。详见 docs/SANDBOX.md §5。"
        );
        return false;
    }
    let probe = Command::new("zfs").arg("version").output();
    let ok = match probe {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            let o2 = Command::new("zfs").arg("--version").output();
            matches!(o2, Ok(o2) if o2.status.success()) || {
                eprintln!(
                    "[replication_real] SKIP: `zfs version` 退出码非 0（可能内核 ZFS 模块\
                     未加载或非 root）。stderr: {}",
                    String::from_utf8_lossy(&o.stderr)
                );
                false
            }
        }
        Err(e) => {
            eprintln!("[replication_real] SKIP: spawn `zfs version` 失败：{e}");
            return false;
        }
    };
    if !ok {
        return false;
    }
    let uid = Command::new("id").arg("-u").output();
    match uid {
        Ok(o) if String::from_utf8_lossy(&o.stdout).trim() == "0" => true,
        _ => {
            eprintln!(
                "[replication_real] SKIP: 非 root（zpool create / zfs send-recv 需 root）。\
                 跑法：sudo cargo test -p os-storage --test replication_real -- --ignored"
            );
            false
        }
    }
}

/// RAII 销毁池 + 删 sparse file（即使断言失败也清理）。
///
/// Drop 不能 async，用同步 std::process::Command 直接调 `zpool destroy -f`（不走
/// tokio::process）——Drop 在 tokio::test 的 runtime 线程内执行时再 block_on 建嵌套
/// runtime 会 panic。teardown 只需把池/文件清掉，直接 spawn zpool 子进程最简单可靠。
struct RealPoolGuard {
    pool: String,
    vdev: String,
}

impl Drop for RealPoolGuard {
    fn drop(&mut self) {
        let _ = Command::new("zpool")
            .args(["destroy", "-f", &self.pool])
            .status();
        let _ = std::fs::remove_file(&self.vdev);
    }
}

/// 跑一个 shell 命令（root 已确认），返回 (success, combined_output)。
fn sh(script: &str) -> (bool, String) {
    let out = Command::new("bash").arg("-c").arg(script).output();
    match out {
        Ok(o) => {
            let mut s = String::new();
            s.push_str(&String::from_utf8_lossy(&o.stdout));
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            (o.status.success(), s)
        }
        Err(e) => (false, format!("spawn 失败：{e}")),
    }
}

/// 建稀疏文件 vdev + zpool create，返回 guard（drop 自动 destroy）。
fn build_pool(tag: &str) -> Option<(RealPoolGuard, String)> {
    let pool_name = unique_pool(tag);
    let vdev_path = unique_vdev(tag);
    eprintln!("[replication_real] 池={pool_name} vdev={vdev_path}（sparse {VDEV_SIZE}）");
    let truncate = Command::new("truncate")
        .args(["-s", VDEV_SIZE, &vdev_path])
        .status();
    match truncate {
        Ok(s) if s.success() => {}
        other => {
            eprintln!("[replication_real] 建 sparse vdev 失败: {other:?}");
            return None;
        }
    }
    let _guard = RealPoolGuard {
        pool: pool_name.clone(),
        vdev: vdev_path.clone(),
    };
    let (ok, err) = sh(&format!("zpool create -f {pool_name} {vdev_path} 2>&1"));
    if !ok {
        eprintln!("[replication_real] zpool create 失败: {err}");
        return None;
    }
    Some((_guard, pool_name))
}

// ============================================================================
// A. 命令构造验证测（默认跑，纯逻辑）—— ZfsSendRecv
// ============================================================================

/// `send_argv` 构造 `zfs send <snapshot>` argv 正确。
#[test]
fn send_argv_constructs_correctly() {
    let snap = SnapshotId::new("tank/media@s1");
    let (program, argv) = ZfsSendRecv::send_argv(&snap);
    assert_eq!(program, "zfs");
    assert_eq!(argv, vec!["send", "tank/media@s1"]);
}

/// `recv_argv` 本地模式（host=None）构造 `zfs recv <dataset>` argv 正确。
#[test]
fn recv_argv_local_constructs_correctly() {
    let r = ZfsSendRecv::new("root");
    let (program, argv) = r.recv_argv(None, "tank/recv");
    assert_eq!(program, "zfs");
    assert_eq!(argv, vec!["recv", "tank/recv"]);
}

/// `recv_argv` 远端模式构造 `ssh <user>@<host> zfs recv <dataset>` argv 正确。
///
/// 验证 ssh 部分：program=ssh，首参是 `user@host`，后接 `zfs recv <dataset>`。
#[test]
fn recv_argv_remote_constructs_correctly() {
    // 自定义 ssh_user（验证 ssh_user 被正确拼进 argv）
    let r = ZfsSendRecv::new("backup");
    let (program, argv) = r.recv_argv(Some("os-node-2"), "tank/recv");
    assert_eq!(program, "ssh");
    assert_eq!(
        argv,
        vec!["backup@os-node-2", "zfs", "recv", "tank/recv"],
        "远端 recv 应为 `ssh <user>@<host> zfs recv <dataset>`"
    );

    // 默认 ssh_user（root）
    let r_default = ZfsSendRecv::default();
    let (program, argv) = r_default.recv_argv(Some("10.0.0.5"), "backup/media");
    assert_eq!(program, "ssh");
    assert_eq!(argv[0], "root@10.0.0.5");
    assert_eq!(&argv[1..], &["zfs", "recv", "backup/media"]);
}

/// 管道拼接逻辑：send | ssh recv 的完整命令链正确。
///
/// 生产用 `zfs send <snap> | ssh <host> zfs recv <target>` 管道。本测验证把
/// `send_argv` 和 `recv_argv` 拼成 shell 管道命令串的格式符合预期（program + argv
/// 顺序、管道符位置）—— 呼应 replication_impl.rs 文档里描述的命令链。
#[test]
fn send_recv_pipeline_chain_constructs_correctly() {
    let snap = SnapshotId::new("tank/media@daily-2026");
    let r = ZfsSendRecv::new("repl");

    // 源端 send argv
    let (send_prog, send_args) = ZfsSendRecv::send_argv(&snap);
    // 远端 recv argv
    let (recv_prog, recv_args) = r.recv_argv(Some("dr-1"), "tank/recv");

    // 模拟实现里 spawn 管道的命令链（shell 表示，仅用于验证拼接逻辑）。
    let send_str = format!("{} {}", send_prog, send_args.join(" "));
    let recv_str = format!("{} {}", recv_prog, recv_args.join(" "));
    let pipeline = format!("{send_str} | {recv_str}");

    assert_eq!(send_str, "zfs send tank/media@daily-2026");
    assert_eq!(recv_str, "ssh repl@dr-1 zfs recv tank/recv");
    assert_eq!(
        pipeline,
        "zfs send tank/media@daily-2026 | ssh repl@dr-1 zfs recv tank/recv"
    );
}

// ============================================================================
// A. 命令构造验证测（默认跑，纯逻辑）—— ZfsNativeCrypto（capture runner）
// ============================================================================

/// 捕获型 CommandRunner：记录最后一次 run 的 (program, args)，返回 exit 0。
/// 用于验证 ZfsNativeCrypto 构造的 argv 是否符合 zfs CLI 约定（passphrase 不落 argv）。
mod capture_runner {
    use async_trait::async_trait;
    use os_core::CommandOutput;
    use os_storage::{CommandRunner, StorageError};
    use std::sync::{Arc, Mutex};

    /// 捕获记录（program, args）。
    pub type Capture = Arc<Mutex<Vec<(String, Vec<String>)>>>;

    pub struct CaptureRunner {
        pub calls: Capture,
    }

    #[async_trait]
    impl CommandRunner for CaptureRunner {
        async fn run(&self, program: &str, args: &[String]) -> Result<CommandOutput, StorageError> {
            self.calls
                .lock()
                .unwrap()
                .push((program.to_string(), args.to_vec()));
            Ok(CommandOutput::ok())
        }
    }

    pub fn new() -> (CaptureRunner, Capture) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            CaptureRunner {
                calls: calls.clone(),
            },
            calls,
        )
    }
}

/// crypto `encrypt_dataset` 命令构造：`zfs change-key -o encryption=...
/// -o keyformat=passphrase -o keylocation=prompt <dataset>`，**passphrase 不落 argv**
/// （敏感数据不应出现在命令行参数里——规格书 §3 明确，应经 stdin/keylocation 注入）。
#[tokio::test]
async fn crypto_encrypt_dataset_argv_no_passphrase_in_args() {
    let (runner, calls) = capture_runner::new();
    let c = ZfsNativeCrypto::with_runner(Box::new(runner));

    let passphrase = "super-secret-passphrase-12345";
    c.encrypt_dataset(&DatasetId::new("vault/secret"), passphrase)
        .await
        .unwrap();

    let captured = calls.lock().unwrap().clone();
    assert_eq!(captured.len(), 1, "应仅调一次 zfs 命令");
    let (program, args) = &captured[0];
    assert_eq!(program, "zfs");
    assert_eq!(args[0], "change-key", "encrypt_dataset 用 change-key");
    assert!(
        args.iter().any(|a| a == "encryption=aes-256-gcm"),
        "应指定 encryption=aes-256-gcm: {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "keyformat=passphrase"),
        "应指定 keyformat=passphrase: {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "keylocation=prompt"),
        "应指定 keylocation=prompt（stdin 注入）: {args:?}"
    );
    assert!(
        args.iter().any(|a| a == "vault/secret"),
        "argv 应含目标 dataset 名: {args:?}"
    );
    // 关键：passphrase 绝不能出现在 argv（应经 stdin / keylocation 文件注入）。
    assert!(
        !args.iter().any(|a| a.contains(passphrase)),
        "passphrase 不应出现在 argv（敏感数据，应经 stdin）: {args:?}"
    );
}

/// crypto `load_key`/`unload_key`/`change_key` 命令构造验证。
#[tokio::test]
async fn crypto_key_ops_argv_constructs_correctly() {
    let (runner, calls) = capture_runner::new();
    let c = ZfsNativeCrypto::with_runner(Box::new(runner));

    c.load_key(&DatasetId::new("vault/secret"), "pass1")
        .await
        .unwrap();
    c.unload_key(&DatasetId::new("vault/secret")).await.unwrap();
    c.change_key(&DatasetId::new("vault/secret"), "newpass")
        .await
        .unwrap();

    let captured = calls.lock().unwrap().clone();
    assert_eq!(captured.len(), 3);

    // load-key
    assert_eq!(captured[0].0, "zfs");
    assert_eq!(captured[0].1, vec!["load-key", "vault/secret"]);

    // unload-key
    assert_eq!(captured[1].0, "zfs");
    assert_eq!(captured[1].1, vec!["unload-key", "vault/secret"]);

    // change-key
    assert_eq!(captured[2].0, "zfs");
    assert_eq!(captured[2].1, vec!["change-key", "vault/secret"]);

    // 三个操作的 passphrase 都不应出现在任何 argv。
    for (_, args) in &captured {
        assert!(
            !args
                .iter()
                .any(|a| a.contains("pass1") || a.contains("newpass")),
            "passphrase 不应出现在 argv: {args:?}"
        );
    }
}

// ============================================================================
// B. 真实 zfs send-recv 往返测（#[ignore]，需 root + zfs）
// ============================================================================

/// 本地 `zfs send <snap> | zfs recv <target>` 往返：建临时池 → create dataset →
/// 写测试数据 → snapshot → send | recv → 验证 target dataset 有数据。
///
/// 用独立测试池（不碰 osprobepersist——LIO export 过的 pool 上做 send-recv 可能触发
/// batch5 发现的 ZFS/LIO 内核挂起）。
#[tokio::test]
#[ignore = "真实 zfs send-recv：需 root + zfsutils-linux + zfs 模块。跑法：sudo cargo test --test replication_real -- --ignored --nocapture"]
async fn real_local_send_recv_roundtrip() {
    if !real_zfs_ready() {
        return;
    }
    let (guard, pool) = match build_pool("srpipe") {
        Some(x) => x,
        None => return,
    };
    let src = format!("{pool}/src");
    let dst = format!("{pool}/dst");

    // 1. create src dataset + 写测试数据
    let (ok, err) = sh(&format!("zfs create {src}"));
    assert!(ok, "create src 失败: {err}");
    let mountpoint = sh(&format!("zfs get -H -o value mountpoint {src}"))
        .1
        .trim()
        .to_string();
    assert!(
        !mountpoint.is_empty() && mountpoint != "-",
        "src mountpoint 应非空: {mountpoint:?}"
    );
    let test_file = format!("{mountpoint}/hello.txt");
    let (ok, err) = sh(&format!(
        "echo 'send-recv-roundtrip-{}' > {test_file}",
        std::process::id()
    ));
    assert!(ok, "写测试数据失败: {err}");

    // 2. snapshot
    let snap_full = format!("{src}@snap1");
    let (ok, err) = sh(&format!("zfs snapshot {snap_full}"));
    assert!(ok, "snapshot 失败: {err}");

    // 3. zfs send | zfs recv（本地管道）
    let (ok, err) = sh(&format!("zfs send {snap_full} | zfs recv -F {dst}"));
    assert!(ok, "send | recv 失败: {err}");

    // 4. 验证 dst dataset 存在 + 数据可读（-F recv 后 dst 已挂载）
    let dst_mountpoint = sh(&format!("zfs get -H -o value mountpoint {dst}"))
        .1
        .trim()
        .to_string();
    assert!(
        !dst_mountpoint.is_empty() && dst_mountpoint != "-",
        "dst mountpoint 应非空: {dst_mountpoint:?}"
    );
    let dst_file = format!("{dst_mountpoint}/hello.txt");
    let content = sh(&format!("cat {dst_file}")).1;
    assert!(
        content.contains(&format!("send-recv-roundtrip-{}", std::process::id())),
        "dst 应有源端写入的数据，实际 cat: {content}"
    );
    eprintln!("[replication_real] 本地 send|recv 往返 OK：dst={dst_mountpoint} 含数据");

    drop(guard);
}

/// `zfs send` 到文件 + 从文件 `zfs recv`：send > /tmp/snap.stream → recv < stream。
/// 更安全的往返验证（不涉及管道并发；接近真实 ssh 跨机场景的本地模拟）。
#[tokio::test]
#[ignore = "真实 zfs send 到文件 + recv：需 root + zfsutils-linux + zfs 模块。跑法：sudo cargo test --test replication_real -- --ignored --nocapture"]
async fn real_send_to_file_recv_from_file_roundtrip() {
    if !real_zfs_ready() {
        return;
    }
    let (guard, pool) = match build_pool("srfile") {
        Some(x) => x,
        None => return,
    };
    let src = format!("{pool}/src");
    let dst = format!("{pool}/dst");
    let stream = format!(
        "/tmp/{POOL_PREFIX}_stream_{}_{}.bin",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );

    let (ok, err) = sh(&format!("zfs create {src}"));
    assert!(ok, "create src 失败: {err}");
    let mountpoint = sh(&format!("zfs get -H -o value mountpoint {src}"))
        .1
        .trim()
        .to_string();
    let marker = format!(
        "file-roundtrip-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let (ok, err) = sh(&format!("echo '{marker}' > {mountpoint}/data.txt"));
    assert!(ok, "写数据失败: {err}");

    let snap_full = format!("{src}@s1");
    let (ok, err) = sh(&format!("zfs snapshot {snap_full}"));
    assert!(ok, "snapshot 失败: {err}");

    // send 到文件
    let (ok, err) = sh(&format!("zfs send {snap_full} > {stream}"));
    assert!(ok, "send 到文件失败: {err}");
    let stream_size = sh(&format!("stat -c %s {stream}")).1.trim().to_string();
    let size: u64 = stream_size.parse().unwrap_or(0);
    assert!(
        size > 0,
        "stream 文件应非空（zfs send 产出元数据+数据）: {stream_size}"
    );

    // 从文件 recv
    let (ok, err) = sh(&format!("zfs recv -F {dst} < {stream}"));
    assert!(ok, "recv 从文件失败: {err}");

    // 验证 dst 数据
    let dst_mountpoint = sh(&format!("zfs get -H -o value mountpoint {dst}"))
        .1
        .trim()
        .to_string();
    let content = sh(&format!("cat {dst_mountpoint}/data.txt")).1;
    assert!(
        content.contains(&marker),
        "dst 应有源端 marker，实际: {content}"
    );
    eprintln!("[replication_real] send→file→recv 往返 OK：stream={size}B dst 含 marker");

    let _ = std::fs::remove_file(&stream);
    drop(guard);
}

/// 增量 send-recv：snap1 → 改数据 → snap2 → `zfs send -i snap1 snap2 | zfs recv`
/// 验证增量流正确还原 snap2 状态。
#[tokio::test]
#[ignore = "真实 zfs 增量 send-recv：需 root + zfsutils-linux + zfs 模块。跑法：sudo cargo test --test replication_real -- --ignored --nocapture"]
async fn real_incremental_send_recv() {
    if !real_zfs_ready() {
        return;
    }
    let (guard, pool) = match build_pool("srincr") {
        Some(x) => x,
        None => return,
    };
    let src = format!("{pool}/src");
    let dst = format!("{pool}/dst");

    let (ok, err) = sh(&format!("zfs create {src}"));
    assert!(ok, "create src 失败: {err}");
    let mountpoint = sh(&format!("zfs get -H -o value mountpoint {src}"))
        .1
        .trim()
        .to_string();

    // snap1（含 v1.txt）
    let (ok, err) = sh(&format!(
        "echo 'v1-{}' > {mountpoint}/v1.txt",
        std::process::id()
    ));
    assert!(ok, "写 v1 失败: {err}");
    let snap1 = format!("{src}@s1");
    let (ok, err) = sh(&format!("zfs snapshot {snap1}"));
    assert!(ok, "snapshot s1 失败: {err}");

    // 全量 send snap1 → dst（增量 send 的前提：dst 必须先有 snap1 的全量）
    let (ok, err) = sh(&format!("zfs send {snap1} | zfs recv -F {dst}"));
    assert!(ok, "全量 send snap1 → dst 失败: {err}");

    // 改数据（加 v2.txt）→ snap2
    let marker2 = format!(
        "v2-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );
    let (ok, err) = sh(&format!("echo '{marker2}' > {mountpoint}/v2.txt"));
    assert!(ok, "写 v2 失败: {err}");
    let snap2 = format!("{src}@s2");
    let (ok, err) = sh(&format!("zfs snapshot {snap2}"));
    assert!(ok, "snapshot s2 失败: {err}");

    // 增量 send snap2（基于 snap1）
    let (ok, err) = sh(&format!("zfs send -i {snap1} {snap2} | zfs recv -F {dst}"));
    assert!(ok, "增量 send-recv 失败: {err}");

    // 验证 dst 现在同时有 v1 和 v2（snap2 状态）
    let dst_mountpoint = sh(&format!("zfs get -H -o value mountpoint {dst}"))
        .1
        .trim()
        .to_string();
    let v1 = sh(&format!("cat {dst_mountpoint}/v1.txt")).1;
    let v2 = sh(&format!("cat {dst_mountpoint}/v2.txt")).1;
    assert!(
        v1.contains(&format!("v1-{}", std::process::id())),
        "dst v1 缺失: {v1}"
    );
    assert!(v2.contains(&marker2), "dst v2 缺失/marker 不符: {v2}");
    eprintln!("[replication_real] 增量 send-recv OK：dst 含 v1+v2（snap2 状态）");

    drop(guard);
}

/// crypto 加密数据集 + passphrase 注入：`zfs create -o encryption=on
/// -o keyformat=passphrase -o keylocation=prompt <ds>`，passphrase 经 stdin。
/// 验证：加密数据集可建、密钥加载状态可查询、unload 后需 load 才能访问。
#[tokio::test]
#[ignore = "真实 zfs crypto：需 root + zfsutils-linux + zfs 模块（含 encryption feature）。跑法：sudo cargo test --test replication_real -- --ignored --nocapture"]
async fn real_crypto_encrypted_dataset_with_passphrase() {
    if !real_zfs_ready() {
        return;
    }
    // 加密 feature 探测：建一个小测，若 zfs 不支持加密则 SKIP（极少数发行版裁剪了
    // encryption feature）。`zfs create -o encryption=on` 不支持时会非零退出 + 报错。
    let (guard, pool) = match build_pool("srencrypt") {
        Some(x) => x,
        None => return,
    };
    let ds = format!("{pool}/vault");

    // 1. 建加密数据集（passphrase 经 stdin，`-` 表示读 stdin）
    let passphrase = "real-crypto-passphrase-2026";
    let (ok, err) = sh(&format!(
        "echo '{passphrase}' | zfs create -o encryption=on -o keyformat=passphrase \
         -o keylocation=prompt {ds}"
    ));
    if !ok {
        let lower = err.to_lowercase();
        if lower.contains("encryption")
            || lower.contains("unsupported")
            || lower.contains("feature")
        {
            eprintln!(
                "[replication_real] SKIP crypto：zfs 不支持 encryption feature（裁剪版）。stderr: {err}"
            );
            drop(guard);
            return;
        }
        panic!("[replication_real] 建加密数据集失败: {err}");
    }
    eprintln!("[replication_real] 加密数据集 {ds} 建立成功");

    // 2. 验证加密属性
    let encryption_val = sh(&format!("zfs get -H -o value encryption {ds}"))
        .1
        .trim()
        .to_string();
    assert!(
        !encryption_val.is_empty() && encryption_val != "-",
        "encryption 属性应有值: {encryption_val:?}"
    );
    eprintln!("[replication_real] encryption={encryption_val}");

    // 3. 写数据 + unload-key 后数据不可访问
    let mountpoint = sh(&format!("zfs get -H -o value mountpoint {ds}"))
        .1
        .trim()
        .to_string();
    let marker = format!("crypto-data-{}", std::process::id());
    let (ok, err) = sh(&format!("echo '{marker}' > {mountpoint}/secret.txt"));
    assert!(ok, "写加密数据失败: {err}");

    // 4. unload-key（先 umount，再 unload-key）
    let (ok, err) = sh(&format!("zfs umount {ds} && zfs unload-key {ds}"));
    assert!(ok, "umount + unload-key 失败: {err}");

    // 5. unload 后 keystatus=unavailable
    let status = sh(&format!("zfs get -H -o value keystatus {ds}"))
        .1
        .trim()
        .to_string();
    assert_eq!(
        status, "unavailable",
        "unload-key 后 keystatus 应=unavailable，实际: {status}"
    );

    // 6. load-key（passphrase 经 stdin）→ mount → 数据可读
    let (ok, err) = sh(&format!("echo '{passphrase}' | zfs load-key {ds}"));
    assert!(ok, "load-key 失败: {err}");
    let (ok, err) = sh(&format!("zfs mount {ds}"));
    assert!(ok, "load-key 后 mount 失败: {err}");

    let status = sh(&format!("zfs get -H -o value keystatus {ds}"))
        .1
        .trim()
        .to_string();
    assert_eq!(status, "available", "load-key 后 keystatus 应=available");
    let content = sh(&format!("cat {mountpoint}/secret.txt")).1;
    assert!(
        content.contains(&marker),
        "load-key 后数据应可读，实际: {content}"
    );
    eprintln!("[replication_real] crypto unload/load-key 往返 OK：数据访问受密钥控制");

    drop(guard);
}
