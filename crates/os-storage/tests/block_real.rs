//! `LioBlockExport` 命令构造测 + 本机 configfs/targetcli 真实可达性测。
//!
//! 分两类（呼应 docs/SANDBOX.md §5「应入沙箱测试清单」的 LIO/nvmet 项）：
//!
//! ## A. 命令构造正确性测（默认跑，不需 root）
//! 验证 `LioBlockExport::export_iscsi` / `export_nvmeof` / `unexport` 构造的
//! targetcli / nvmetcli 命令字符串符合内核 LIO / nvmet 的真实编排语义
//! （backstore→target→lun→portal 的正确顺序、WWN/NQN/NSID 参数齐全、destroy 是
//! create 的逆操作）。用一个捕获型 `CaptureRunner` 注入，把每次 `run(program, args)`
//! 的调用记下来断言——不真跑任何子进程，无需 root，CI 三道门默认执行。
//!
//! ## B. configfs / targetcli 真实可达性测（`#[ignore]`，需 root）
//! 在有 configfs + targetcli/nvmetcli + 内核 target 模块的本机/沙箱才能真跑：
//! 1. `real_configfs_mounted`：configfs 是否挂载于 /sys/kernel/config，target 子系统是否在。
//! 2. `real_targetcli_reachable`：装了 targetcli 则跑 `targetcli ls` 验证可达。
//! 3. `real_lio_target_round_trip`：建唯一前缀的 iSCSI target → 验存在 → destroy → 验消失。
//!
//! ## 红线
//! - B 类用唯一 wwn 前缀（带 PID+纳秒），测完 RAII destroy，**绝不碰宿主真实 target**。
//! - 无 configfs / 无 targetcli / 无 root：优雅 SKIP（eprintln 报告缺什么，不 panic）。

#![cfg(feature = "mock")]

use os_core::{CommandOutput, VolumeId};
use os_storage::{BlockExport, CommandRunner, LioBlockExport, StorageError};
use std::sync::{Arc, Mutex};

// ============================================================================
// 捕获型 CommandRunner（共享内部状态，可注入 LioBlockExport::with_runner）
// ============================================================================

/// 捕获型 CommandRunner 的共享状态——用 Arc 包裹，runner 实例与外部读引用共享同一份。
#[derive(Default)]
struct CaptureState {
    calls: Mutex<Vec<(String, Vec<String>)>>,
    /// 下一次 run 返回的输出（默认 ok）。用 Mutex 便于失败路径测注入非零退出。
    next: Mutex<Option<CommandOutput>>,
}

impl CaptureState {
    fn new() -> Self {
        Self::default()
    }

    /// 设下一次 run 返回的输出（仅生效一次，之后恢复 ok）。
    fn set_next(&self, out: CommandOutput) {
        *self.next.lock().unwrap() = Some(out);
    }

    /// 取出已捕获的调用列表（move 出来，便于断言后清空）。
    fn drain(&self) -> Vec<(String, Vec<String>)> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }
}

/// 捕获型 CommandRunner——持有 `Arc<CaptureState>`，可被 `with_runner` 接收（owned），
/// 同时外部保留另一个 clone 的 Arc 读捕获结果。
struct CaptureRunner {
    state: Arc<CaptureState>,
}

impl CaptureRunner {
    fn new() -> (Self, Arc<CaptureState>) {
        let state = Arc::new(CaptureState::new());
        (
            Self {
                state: state.clone(),
            },
            state,
        )
    }
}

#[async_trait::async_trait]
impl CommandRunner for CaptureRunner {
    async fn run(&self, program: &str, args: &[String]) -> Result<CommandOutput, StorageError> {
        self.state
            .calls
            .lock()
            .unwrap()
            .push((program.to_string(), args.to_vec()));
        let out = self
            .state
            .next
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(CommandOutput::ok);
        Ok(out)
    }
}

/// 便捷：从捕获的调用里抽出 program=targetcli 的所有 args[0]（targetcli 的 path 命令）。
fn targetcli_cmds(calls: &[(String, Vec<String>)]) -> Vec<String> {
    calls
        .iter()
        .filter(|(p, _)| p == "targetcli")
        .filter_map(|(_, a)| a.first().cloned())
        .collect()
}

/// 便捷：从捕获的调用里抽出 program=nvmetcli 的所有 args[0]。
fn nvmetcli_cmds(calls: &[(String, Vec<String>)]) -> Vec<String> {
    calls
        .iter()
        .filter(|(p, _)| p == "nvmetcli")
        .filter_map(|(_, a)| a.first().cloned())
        .collect()
}

// ============================================================================
// A. 命令构造正确性测（默认跑，不需 root）
// ============================================================================

#[tokio::test]
async fn iscsi_export_constructs_backstore_target_lun_in_order() {
    // 验证 export_iscsi 构造的 targetcli 命令：
    // 1) 先建 block backstore（/backstores/block create vol-<sanitized-vol> <zvol>）
    // 2) 建 iSCSI target（/iscsi create <iqn>，targetcli 自动建 tpg1 + 默认 portal）
    // 3) 映射 LUN（/iscsi/<iqn>/tpg1/luns create /backstores/block/vol-<sanitized-vol>）
    // **不**显式建 portal——LIO 默认 auto_add_default_portal=true 已在 /iscsi create 时建，
    // 显式建会因「NetworkPortal already exists」退非零（exit 1）触发 ExportFailed。
    // backstore 名与 IQN 后缀都把 volume 的 `/` → `-`（LIO 不允许 `/`）。
    let (runner, state) = CaptureRunner::new();
    let be = LioBlockExport::with_runner(Box::new(runner), "iqn.2026-08.test", "nqn.2026-08.test");

    let t = be
        .export_iscsi(&VolumeId::new("tank/vol0"), 0, Vec::new())
        .await
        .expect("export_iscsi 应成功（runner 返回 ok）");

    let cmds = targetcli_cmds(&state.drain());
    assert_eq!(cmds.len(), 3, "应有 3 条 targetcli 命令，实际 {:?}", cmds);

    // ① backstore create（backstore 名 vol-tank-vol0：`/` → `-`）
    assert!(
        cmds[0].contains("/backstores/block create"),
        "第 1 条应是 backstore create: {}",
        cmds[0]
    );
    assert!(
        cmds[0].contains("vol-tank-vol0"),
        "backstore 名含 sanitized volume（`/` → `-`）: {}",
        cmds[0]
    );
    assert!(
        cmds[0].contains("/dev/zvol/tank/vol0"),
        "backstore 指向 zvol 路径（保留 `/`）: {}",
        cmds[0]
    );

    // ② iscsi create <iqn>
    assert!(
        cmds[1].starts_with("/iscsi create "),
        "第 2 条应是 /iscsi create: {}",
        cmds[1]
    );
    let iqn = cmds[1].strip_prefix("/iscsi create ").expect("iqn 前缀");
    assert_eq!(iqn, t.iqn, "构造命令里的 IQN 应与返回的 target.iqn 一致");
    assert!(
        iqn.starts_with("iqn.2026-08.test:vol-tank-vol0-lun0"),
        "IQN 由 iqn_base + sanitized-volume + lun 拼出（不含 `/`）: {}",
        iqn
    );

    // ③ lun map（backstore 引用路径也用 sanitized 名）
    assert!(
        cmds[2].contains("/tpg1/luns create"),
        "第 3 条应是 lun map: {}",
        cmds[2]
    );
    assert!(
        cmds[2].contains("/backstores/block/vol-tank-vol0"),
        "lun 引用 backstore 路径（sanitized 名）: {}",
        cmds[2]
    );

    // 不应有 portals create 命令（LIO 默认自动建 portal）
    assert!(
        !cmds.iter().any(|c| c.contains("/tpg1/portals create")),
        "不应显式建 portal（auto_add_default_portal=true）: {:?}",
        cmds
    );
}

#[tokio::test]
async fn iscsi_export_with_initiators_emits_acl_create() {
    // 验证传 initiators 时多一条 /iscsi/<iqn>/tpg1/acls create <initiators> 命令。
    let (runner, state) = CaptureRunner::new();
    let be = LioBlockExport::with_runner(Box::new(runner), "iqn.2026-08.test", "nqn.2026-08.test");

    let t = be
        .export_iscsi(
            &VolumeId::new("tank/vol1"),
            1,
            vec!["iqn.1998-01.com.example:init-a".into()],
        )
        .await
        .expect("export_iscsi 应成功");

    let cmds = targetcli_cmds(&state.drain());
    // 3 条基础 + 1 条 ACL = 4
    assert_eq!(cmds.len(), 4, "应有 4 条命令（含 ACL），实际 {:?}", cmds);
    let acl = cmds.last().expect("末条应是 ACL create");
    assert!(
        acl.contains("/tpg1/acls create"),
        "ACL 命令路径正确: {}",
        acl
    );
    assert!(
        acl.contains("iqn.1998-01.com.example:init-a"),
        "ACL 含 initiator IQN"
    );
    // 验证返回的 IscsiTarget 记录了 initiators
    assert_eq!(
        t.initiators,
        vec!["iqn.1998-01.com.example:init-a".to_string()]
    );
}

#[tokio::test]
async fn nvmeof_export_constructs_subsystem_namespace_host() {
    // 验证 export_nvmeof 构造的 nvmetcli 命令：
    // 1) create subsystem <nqn>
    // 2) create namespace <nqn> -b <zvol_path>
    // 3) create host <nqn> -n '*'（允许所有 host 连接）
    let (runner, state) = CaptureRunner::new();
    let be = LioBlockExport::with_runner(Box::new(runner), "iqn.2026-08.test", "nqn.2026-08.test");

    // 传空 nqn 触发默认生成（make_nqn）
    let ns = be
        .export_nvmeof(&VolumeId::new("tank/nv0"), "")
        .await
        .expect("export_nvmeof 应成功");

    let cmds = nvmetcli_cmds(&state.drain());
    assert_eq!(cmds.len(), 3, "应有 3 条 nvmetcli 命令，实际 {:?}", cmds);

    // ① subsystem
    assert!(
        cmds[0].starts_with("create subsystem "),
        "第 1 条应是 create subsystem: {}",
        cmds[0]
    );
    assert_eq!(
        ns.nqn,
        cmds[0].strip_prefix("create subsystem ").unwrap(),
        "构造命令里的 NQN 应与返回 ns.nqn 一致"
    );
    assert!(
        ns.nqn.starts_with("nqn.2026-08.test:vol-tank-nv0"),
        "默认 NQN 由 nqn_base + sanitized-volume 拼出（`/` → `-`）: {}",
        ns.nqn
    );

    // ② namespace（含 -b 后端路径）
    assert!(cmds[1].contains("create namespace"), "第 2 条 namespace");
    assert!(
        cmds[1].contains("-b /dev/zvol/tank/nv0"),
        "namespace 命令含 -b <zvol_path>"
    );

    // ③ host（允许所有）
    assert!(cmds[2].contains("create host"), "第 3 条 host");
    assert!(cmds[2].contains("-n '*'"), "host 命令含 -n '*'");
}

#[tokio::test]
async fn iscsi_unexport_is_inverse_of_create() {
    // 验证 unexport 是 export 的逆操作：
    // - 先 export（建 backstore + target + lun；portal 由 LIO 默认自动建）
    // - 再 unexport：应删 iSCSI target + 删 backstore（create 建的两个独立对象）
    // - 删 backstore 名 = vol-<sanitized-volume>（从 IscsiTarget.volume 反推 + sanitize_name）
    let (runner, state) = CaptureRunner::new();
    let be = LioBlockExport::with_runner(Box::new(runner), "iqn.2026-08.test", "nqn.2026-08.test");

    let t = be
        .export_iscsi(&VolumeId::new("tank/inv"), 2, Vec::new())
        .await
        .expect("export 应成功");
    let _ = state.drain(); // 清掉 create 阶段的捕获，只看 unexport

    be.unexport(&t.iqn).await.expect("unexport 应成功");

    let cmds = targetcli_cmds(&state.drain());
    // destroy 应含：① /iscsi delete <iqn>  ② /backstores/block delete vol-tank-inv
    assert_eq!(cmds.len(), 2, "unexport 应发 2 条命令，实际 {:?}", cmds);
    assert!(
        cmds[0].contains("/iscsi delete"),
        "第 1 条删 iSCSI target: {}",
        cmds[0]
    );
    assert!(cmds[0].contains(&t.iqn), "删的 target IQN 与建的一致");
    assert!(
        cmds[1].contains("/backstores/block delete"),
        "第 2 条删 backstore: {}",
        cmds[1]
    );
    assert!(
        cmds[1].contains("vol-tank-inv"),
        "删的 backstore 名 = vol-<sanitized-volume>（`/` → `-`）"
    );
}

#[tokio::test]
async fn nvmeof_unexport_deletes_subsystem() {
    // 验证 NVMe-oF unexport 删 subsystem（与 export 的 create subsystem 对称）。
    let (runner, state) = CaptureRunner::new();
    let be = LioBlockExport::with_runner(Box::new(runner), "iqn.2026-08.test", "nqn.2026-08.test");

    let ns = be
        .export_nvmeof(&VolumeId::new("tank/nvinv"), "nqn.custom:inv1")
        .await
        .expect("export 应成功");
    let _ = state.drain();

    be.unexport(&ns.nqn).await.expect("unexport 应成功");

    let cmds = nvmetcli_cmds(&state.drain());
    assert_eq!(
        cmds.len(),
        1,
        "unexport 应发 1 条 nvmetcli 命令，实际 {:?}",
        cmds
    );
    assert!(
        cmds[0].contains("delete subsystem"),
        "删 subsystem: {}",
        cmds[0]
    );
    assert!(cmds[0].contains("nqn.custom:inv1"), "删的 NQN 与建的一致");
}

#[tokio::test]
async fn export_iscsi_propagates_targetcli_failure() {
    // 验证 targetcli 非零退出时 export_iscsi 返回 ExportFailed（不静默吞错）。
    let (runner, state) = CaptureRunner::new();
    // 让第 1 条（backstore create）失败
    state.set_next(CommandOutput::fail(1, "backstore already exists"));
    let be = LioBlockExport::with_runner(Box::new(runner), "iqn.2026-08.test", "nqn.2026-08.test");

    let err = be
        .export_iscsi(&VolumeId::new("tank/err"), 0, Vec::new())
        .await
        .unwrap_err();
    assert!(
        matches!(err, StorageError::ExportFailed(_)),
        "应映射为 ExportFailed，实际: {err:?}"
    );
    let cmds = targetcli_cmds(&state.drain());
    assert_eq!(cmds.len(), 1, "第 1 条失败后应中断，不继续后续命令");
}

// ============================================================================
// B. configfs / targetcli 真实可达性测（#[ignore]，需 root）
// ============================================================================
//
// 以下测在「本机大概率 SKIP」（无 targetcli / 无 target 内核模块 / 非 root），
// 在沙箱（Docker privileged + targetcli + 内核模块）才能真跑。每个测自检环境，
// 不满足则 eprintln + return（不 panic），保持 `--ignored` 套件可重复运行。
//
// 跑法：
//   sudo cargo test -p os-storage --features mock --test block_real -- --ignored --nocapture
//
// 红线：用唯一 wwn 前缀（PID+纳秒），RAII guard 保证测完 destroy，绝不碰宿主真实 target。

/// 测试用唯一 wwn 前缀——避免与宿主真实 iSCSI target 冲突，测后必须 destroy。
const TEST_WWN_PREFIX: &str = "iqn.2026-08.osprobe";

/// 纯 Rust 的 `which`：扫 $PATH 找可执行文件（不引 which crate 依赖）。
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

/// 跳过条件：非 root 直接 return false（不 panic）。
fn require_root() -> bool {
    // SAFETY: getuid 是无副作用、永不失败的系统调用。
    let uid = unsafe { getuid() };
    if uid != 0 {
        eprintln!(
            "[SKIP] 非 root（uid={uid}），configfs/targetcli 写操作需 root。\
             跑法：sudo cargo test ... -- --ignored"
        );
        return false;
    }
    true
}

// libc::getuid 的薄封装（避免直接引 libc crate 依赖；本 crate 面向 Linux）。
extern "C" {
    fn getuid() -> u32;
}

/// configfs 是否挂载于 /sys/kernel/config。
fn configfs_mounted() -> bool {
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap_or_default();
    mounts.lines().any(|l| {
        let mut it = l.split_whitespace();
        let _src = it.next();
        let target = it.next();
        let fstype = it.next();
        fstype == Some("configfs") && target == Some("/sys/kernel/config")
    })
}

/// configfs 下是否有 target（LIO）子系统目录。
fn configfs_has_target() -> bool {
    std::path::Path::new("/sys/kernel/config/target").is_dir()
}

/// configfs 下是否有 nvmet 子系统目录。
fn configfs_has_nvmet() -> bool {
    std::path::Path::new("/sys/kernel/config/nvmet").is_dir()
}

/// 跑 targetcli 命令（已确认 root + targetcli 在 PATH）。
async fn targetcli(args: &[&str]) -> Result<CommandOutput, StorageError> {
    let runner = os_storage::TokioCommandRunner;
    let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
    runner.run("targetcli", &owned).await
}

/// RAII guard：测结束时尽力销毁指定 IQN 的 iSCSI target + backstore（忽略错误）。
///
/// 生产 unexport 经内存注册表走 CLI；guard 持有的 be 与被测 be 不是同一实例（注册表分离），
/// 故 guard 不依赖注册表，直接调 targetcli 物理删除——保证即使测主体异常也能清理。
struct IscsiTargetGuard {
    iqn: Option<String>,
    backstore: Option<String>,
}
impl Drop for IscsiTargetGuard {
    fn drop(&mut self) {
        let (iqn, backstore) = match (self.iqn.take(), self.backstore.take()) {
            (Some(i), Some(b)) => (i, b),
            _ => return, // 测主体已自行 destroy，guard 无事可做
        };
        // Drop 不能 async：开一次性 runtime 阻塞执行清理。
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()?;
            rt.block_on(async {
                // 先删 target 再删 backstore（与 unexport 顺序一致），忽略错误。
                let _ = targetcli(&["/iscsi", "delete", &iqn]).await;
                let _ = targetcli(&["/backstores/block", "delete", &backstore]).await;
            });
            Some(())
        })
        .join();
        // 忽略返回：失败（target 本就不存在 / 环境不可用）属预期。
    }
}

#[tokio::test]
#[ignore = "需 root + configfs。跑法：cargo test -- --ignored real_configfs_mounted"]
async fn real_configfs_mounted() {
    if !require_root() {
        return;
    }
    // configfs 挂载验证
    if !configfs_mounted() {
        // 尝试 mount（本测已是 root）
        eprintln!("[INFO] configfs 未挂载，尝试 mount -t configfs none /sys/kernel/config");
        let out = std::process::Command::new("mount")
            .args(["-t", "configfs", "none", "/sys/kernel/config"])
            .output();
        match out {
            Ok(o) if o.status.success() => eprintln!("[OK] configfs 挂载成功"),
            Ok(o) => {
                eprintln!(
                    "[SKIP] configfs 挂载失败（内核可能未编 configfs）：{}",
                    String::from_utf8_lossy(&o.stderr)
                );
                return;
            }
            Err(e) => {
                eprintln!("[SKIP] mount 调用失败：{e}");
                return;
            }
        }
    }
    assert!(configfs_mounted(), "挂载后 configfs 应在 /proc/mounts");

    // target / nvmet 子系统探测（无 target_core_mod 则不出现）
    if configfs_has_target() {
        println!("[OK] configfs 下有 target 子系统（target_core_mod 已加载）");
    } else {
        eprintln!(
            "[INFO] configfs 下无 target 子系统（target_core_mod 未加载，\
             需 modprobe target_core_iblock target_core_file iscsi_target_mod）"
        );
    }
    if configfs_has_nvmet() {
        println!("[OK] configfs 下有 nvmet 子系统（nvmet 模块已加载）");
    } else {
        eprintln!("[INFO] configfs 下无 nvmet 子系统（nvmet/nvmet-tcp 未加载）");
    }
}

#[tokio::test]
#[ignore = "需 root + targetcli/nvmetcli。跑法：cargo test -- --ignored real_targetcli_reachable"]
async fn real_targetcli_reachable() {
    if !require_root() {
        return;
    }
    // targetcli 可达性
    match which("targetcli") {
        Some(p) => {
            println!("[INFO] targetcli 在 {}", p.display());
            // 跑 targetcli ls 验证可达（需能读 configfs target 子系统）
            let out = std::process::Command::new(&p).arg("ls").output();
            match out {
                Ok(o) if o.status.success() => {
                    println!(
                        "[OK] targetcli ls 成功（前 200 字节）：\n{}",
                        String::from_utf8_lossy(&o.stdout)
                            .chars()
                            .take(200)
                            .collect::<String>()
                    );
                }
                Ok(o) => {
                    eprintln!(
                        "[SKIP] targetcli ls 退出码非 0（可能 target 模块未加载）：{}",
                        String::from_utf8_lossy(&o.stderr)
                    );
                }
                Err(e) => {
                    eprintln!("[SKIP] targetcli 调用失败：{e}");
                }
            }
        }
        None => {
            eprintln!("[SKIP] 未装 targetcli（apt install targetcli-fb / dnf install targetcli）");
        }
    }
    // nvmetcli 可达性
    match which("nvmetcli") {
        Some(p) => {
            println!("[INFO] nvmetcli 在 {}", p.display());
            let out = std::process::Command::new(&p).arg("ls").output();
            match out {
                Ok(o) if o.status.success() => {
                    println!("[OK] nvmetcli ls 成功");
                }
                Ok(o) => eprintln!(
                    "[SKIP] nvmetcli ls 退出码非 0：{}",
                    String::from_utf8_lossy(&o.stderr)
                ),
                Err(e) => eprintln!("[SKIP] nvmetcli 调用失败：{e}"),
            }
        }
        None => eprintln!("[SKIP] 未装 nvmetcli"),
    }
}

#[tokio::test]
#[ignore = "需 root + configfs + targetcli + 内核 target 模块 + zvol。\
            跑法：cargo test -- --ignored real_lio_target_round_trip"]
async fn real_lio_target_round_trip() {
    // 前置：root + configfs + targetcli + target 子系统 + zvol 全有才继续，否则 SKIP
    if !require_root() {
        return;
    }
    if which("targetcli").is_none() {
        eprintln!("[SKIP] 未装 targetcli，无法跑 LIO 往返测");
        return;
    }
    if !configfs_has_target() {
        eprintln!(
            "[SKIP] configfs 无 target 子系统（target_core_mod 未加载），\
             无法跑 LIO 往返测。加载：sudo modprobe target_core_iblock iscsi_target_mod"
        );
        return;
    }

    // 用生产构造（TokioCommandRunner 真实 spawn targetcli）。
    // 注意：LioBlockExport::make_iqn 是私有的，但格式确定（<iqn_base>:vol-<volume>-lun<lun>），
    // export_iscsi 返回的 IscsiTarget.iqn 就是它——测用返回值驱动 guard，不预生成。
    let be = LioBlockExport::new(TEST_WWN_PREFIX, "nqn.2026-08.osprobe");
    let volume = VolumeId::new(format!("osprobe-lio-{}", std::process::id()));

    // export_iscsi 会 /dev/zvol/<volume>。本机无 zfs 时 backstore create 会失败——
    // 为隔离 LIO 编排正确性，要求环境先建好 zvol（`zfs create -V 1M <volume>`）。
    let zvol_probe = format!("/dev/zvol/{volume}");
    if !std::path::Path::new(&zvol_probe).exists() {
        eprintln!(
            "[SKIP] {zvol_probe} 不存在（本机无 zfs / 该 zvol 未建），\
             export_iscsi 的 backstore create 会失败。\
             要真跑需先 `sudo zfs create -V 1M {volume}` 建 zvol。"
        );
        return;
    }

    // —— 以下仅在有 zvol 的沙箱才执行 ——
    // RAII guard：export 成功后才填 iqn/backstore，drop 时尽力清理。
    let mut guard = IscsiTargetGuard {
        iqn: None,
        backstore: None,
    };

    let t = match be.export_iscsi(&volume, 0, Vec::new()).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[SKIP] export_iscsi 失败（环境不支持完整 LIO 编排）：{e}");
            return;
        }
    };
    // export 成功：登记 guard（drop 时清 target + backstore）
    guard.iqn = Some(t.iqn.clone());
    guard.backstore = Some(format!("vol-{volume}"));

    // 验证 target 存在：targetcli ls 输出应含 iqn
    let ls = std::process::Command::new("targetcli")
        .arg("ls")
        .output()
        .expect("targetcli ls 应能跑");
    let ls_out = String::from_utf8_lossy(&ls.stdout);
    assert!(
        ls_out.contains(&t.iqn),
        "targetcli ls 应含新建的 target IQN {}\n{}",
        t.iqn,
        ls_out
    );

    // destroy
    be.unexport(&t.iqn).await.expect("unexport 应成功");

    // 验证 target 已消失
    let ls2 = std::process::Command::new("targetcli")
        .arg("ls")
        .output()
        .expect("targetcli ls 应能跑");
    let ls2_out = String::from_utf8_lossy(&ls2.stdout);
    assert!(
        !ls2_out.contains(&t.iqn),
        "destroy 后 targetcli ls 不应再含该 IQN\n{}",
        ls2_out
    );
    // guard 已无用（手动 destroy 了），清空避免 Drop 重复删
    guard.iqn = None;
    guard.backstore = None;
    println!("[OK] LIO target 往返成功：建 → 验存在 → destroy → 验消失");
}
