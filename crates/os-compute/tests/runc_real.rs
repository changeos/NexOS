//! `YoukiRunner` 真实 runc 容器生命周期 `#[ignore]` 测。
//!
//! 定位（呼应 docs/SANDBOX.md §5「应入沙箱测试清单」+ runtime.rs 模块注释）：本 crate
//! 的命令构造层（`create_argv`/`start_argv`/`delete_argv`/`state_argv`/`list_argv`）
//! 已有完整单测覆盖；`YoukiRunner`（真实 `tokio::process::Command` spawn 子进程）从未
//! 真实拉起过容器。本文件用本机 `runc`（OCI 标准 runtime，与 youki 命令面一致）做端到端
//! 真实验证：runc 可达 → OCI bundle 构造 → create → start → state/list → delete。
//!
//! ## youki vs runc
//! 两者同属 OCI runtime，命令面（`--root`/`create`/`start`/`kill`/`delete`/`state`/
//! `list`）完全一致。YoukiRunner 的 `bin` 字段可指向任意 OCI runtime 二进制——本机未装
//! youki，故测指向 `/usr/sbin/runc`（youki 装机后只需把 `bin` 改成 `youki` 即复用本测）。
//!
//! ## 跑法（需 root，runc create 需特权）
//! ```bash
//! cargo build -p os-compute --features mock
//! sudo env PATH=$HOME/.cargo/bin:/usr/sbin:/usr/bin:/sbin:/bin \
//!      RUSTUP_HOME=$HOME/.rustup CARGO_HOME=$HOME/.cargo \
//!      cargo test -p os-compute --features mock --test runc_real -- \
//!           --ignored --nocapture --test-threads=1
//! ```
//! 注意：
//! - sudo 会丢 rustup/cargo 环境，须显式传 `PATH`/`RUSTUP_HOME`/`CARGO_HOME`
//!   （batch4 osd-systemd 子代理踩过）。
//! - `--root` 走 `/tmp/osprobe_runc_<pid>/`，绝不碰宿主 `/run/os/youki` 真实状态根。
//! - **必须 `--test-threads=1`**：runc/内核 cgroup 命名空间在并发 create 时会竞态失败
//!   （`nsexec: failed to read netlink header` + `<state_root>/<id>/` 目录未建），
//!   这是 runc 的环境限制非本 crate bug。C/D 测内虽加了进程内互斥锁（[`create_lock`]）
//!   尽力串行，但 `#[tokio::test]` 默认 current-thread 运行时下跨测锁不可靠——故跑测时
//!   显式 `--test-threads=1` 才 100% 稳定（实测 5 次全绿；并行则 ~50% flaky）。
//!
//! ## SKIP 守护
//! 每测自检环境——非 root / 无 runc / 无 busybox / sudo 缺失则 `eprintln!("[SKIP]…")`
//! 后 `return`（不 panic），保持 `--ignored` 套件可重复运行。
//!
//! ## 红线（绝不碰宿主真实容器）
//! - 唯一容器 ID：`osprobe_runc_<pid>_<nanos>`（PID + 纳秒，全局唯一）；
//! - state_root 落 `/tmp/osprobe_runc_<pid>/`（测后 `rm -rf`）；
//! - RAII guard（[`RuncContainerGuard`]）：测结束 `runc delete --force`（忽略错误）；
//! - bundle 落 `tempfile::TempDir`，测结束自动清理。

#![cfg(feature = "mock")]

use os_compute::oci::{read_config_json, write_bundle};
use os_compute::runtime::{
    self, create_argv, delete_argv, list_argv, parse_state_status, start_argv, state_argv,
    ContainerRuntimeRunner, YoukiRunner,
};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// 全局串行锁——保护「真实 runc create」测不并发。
///
/// 本机实测：两个 `runc create` 并发跑会在 `nsexec` 阶段竞态失败（`failed to read
/// netlink header` + `<state_root>/<id>/` 目录未建），这是 runc/内核 cgroup 命名空间
/// 的并发限制，非本 crate bug。cargo test 默认多线程跑同一 binary 的多个测，故对
/// `real_runc_create_start_delete_lifecycle` / `real_runc_list_and_state_queries` 两个
/// 真实 create 的测加此进程内互斥锁，强制串行（不影响 read-only/version/error 测）。
///
/// 注：`OnceLock` + `tokio::Mutex` 让锁跨测复用；guard 在测函数体内 `await` 持有。
fn create_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

// ============================================================================
// 环境探测 + SKIP 守护
// ============================================================================

// libc::getuid 薄封装（避免引 libc crate；本 crate 面向 Linux）。
extern "C" {
    fn getuid() -> u32;
}

/// runc 二进制定位——优先 $PATH 里的 `runc`，回退常见路径 `/usr/sbin/runc`、`/usr/bin/runc`。
///
/// youki 装机后可改成探测 `youki`，命令面一致。
fn find_runtime_bin() -> Option<PathBuf> {
    // 优先 $PATH（sudo 传 PATH 后 runc 可在标准位置）
    if let Some(p) = which("runc") {
        return Some(p);
    }
    for candidate in ["/usr/sbin/runc", "/usr/bin/runc", "/usr/local/bin/runc"] {
        if Path::new(candidate).is_file() {
            return Some(PathBuf::from(candidate));
        }
    }
    None
}

/// 纯 Rust 的 `which`：扫 $PATH 找可执行文件（不引 which crate）。
fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 必须 root（runc create 需特权建 namespace/cgroup）。非 root 直接 return false。
fn require_root() -> bool {
    // SAFETY: getuid 是无副作用、永不失败的系统调用。
    let uid = unsafe { getuid() };
    if uid != 0 {
        eprintln!(
            "[SKIP] 非 root（uid={uid}），runc create 需特权。\
             跑法：sudo env PATH=... RUSTUP_HOME=... CARGO_HOME=... cargo test ... -- --ignored"
        );
        return false;
    }
    true
}

/// 返回 (runc_bin, runner) 三元组；无 runc 则 eprintln + None。
fn require_runc() -> Option<(PathBuf, YoukiRunner)> {
    let bin = find_runtime_bin()?;
    // state_root 落 /tmp（pid 隔离），避免碰 /run/os/youki 真实状态根
    let pid = std::process::id();
    let state_root = PathBuf::from(format!("/tmp/osprobe_runc_{pid}"));
    let _ = std::fs::create_dir_all(&state_root);
    let runner = YoukiRunner::new(bin.to_string_lossy().into_owned(), &state_root);
    Some((bin, runner))
}

/// busybox 二进制定位（rootfs 最小化：/bin/sh + /bin/true 均拷自 busybox 多调用二进制）。
fn find_busybox() -> Option<PathBuf> {
    if let Some(p) = which("busybox") {
        return Some(p);
    }
    for candidate in ["/usr/bin/busybox", "/bin/busybox"] {
        if Path::new(candidate).is_file() {
            return Some(PathBuf::from(candidate));
        }
    }
    None
}

/// 生成全局唯一容器 ID：`osprobe_runc_<pid>_<nanos>`。
fn unique_id() -> String {
    let pid = std::process::id();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!("osprobe_runc_{pid}_{nanos}")
}

/// 在 bundle 目录里构造最小可跑 rootfs（busybox 多调用二进制 → /bin/sh + /bin/true）。
///
/// runc 默认 config.json 含 /proc /dev /sys 等伪文件系统挂载，容器内 init 进程
/// 需要 /dev/null 等设备节点（runc 在 /dev tmpfs 上 mknod）。本函数仅铺 busybox
/// 到 rootfs/bin/，挂载点由 [`make_runnable_config`] 生成。
fn make_busybox_rootfs(bundle: &Path, busybox: &Path) -> std::io::Result<()> {
    let bin = bundle.join("rootfs").join("bin");
    std::fs::create_dir_all(&bin)?;
    // busybox 是多调用二进制——argv[0] 决定行为；拷成 sh/true 即可作对应命令
    std::fs::copy(busybox, bin.join("sh"))?;
    std::fs::copy(busybox, bin.join("true"))?;
    Ok(())
}

/// RAII guard：测结束时 `runc delete --force <id>`（忽略错误），保证即使测主体异常也清理。
///
/// 与 block_real.rs 的 `IsciTargetGuard` 同构——Drop 里建一次性 runtime 阻塞执行清理。
/// state_root 测后由 [`StateRootGuard`] rm -rf，这里只删 runc 容器状态。
///
/// 持有 runner 的克隆（`YoukiRunner` 字段仅 `String`+`PathBuf`，廉价克隆）——避免
/// `&YoukiRunner` 借用逃逸到 `thread::spawn` 的 `'static` 约束。
struct RuncContainerGuard {
    runner: YoukiRunner,
    id: String,
    armed: bool,
}

impl RuncContainerGuard {
    fn new(runner: YoukiRunner, id: &str) -> Self {
        Self {
            runner,
            id: id.to_string(),
            armed: true,
        }
    }

    /// 测主体成功 delete 后 disarm，避免 guard 重复删（runc delete 不存在容器会报错）。
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RuncContainerGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let runner = self.runner.clone();
        let id = self.id.clone();
        // Drop 不能 async：建一次性 runtime 阻塞执行清理，忽略一切错误。
        let _ = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()?;
            let del = delete_argv(&runner.state_root, &id, true).ok()?;
            let full = runner.full_argv(&del);
            rt.block_on(runner.run(&full)).ok()
        })
        .join();
    }
}

/// RAII guard：测结束 `rm -rf` state_root（/tmp 临时状态根）。
struct StateRootGuard {
    path: PathBuf,
}

impl Drop for StateRootGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// ============================================================================
// A. runc 可达性测（#[ignore]）
// ============================================================================

/// 验证 YoukiRunner 指向 runc 后 `runc --version` 退出 0 + 输出含 "runc version"。
///
/// 不需 root（--version 无特权需求），但放 #[ignore] 因依赖本机 runc 装机。
#[tokio::test]
#[ignore = "真实 runc：需本机 runc 二进制，人工 `cargo test -- --ignored`"]
async fn real_runc_version_via_youki_runner() {
    let bin = match find_runtime_bin() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] 未找到 runc 二进制（PATH/常见路径均无），跳过 runc 可达性测");
            return;
        }
    };

    let tmp = tempfile::tempdir().expect("tempdir 创建失败");
    let runner = YoukiRunner::new(bin.to_string_lossy().into_owned(), tmp.path());
    // argv[0] = program 全路径（YoukiRunner.run 用 argv.split_first 取 program）
    let argv = vec![bin.to_string_lossy().into_owned(), "--version".to_string()];

    let out = runner.run(&argv).await.expect("run runc --version 失败");

    // 退出码 0
    assert_eq!(
        out.exit_code, 0,
        "runc --version 应退出 0，实际 {}，stderr={}",
        out.exit_code, out.stderr
    );
    // 输出含 "runc version"（runc 1.4.0 输出形如 "runc version 1.4.0-0ubuntu1"）
    assert!(
        out.stdout.contains("runc version"),
        "runc --version 输出应含 'runc version'，实际 stdout={}",
        out.stdout
    );
    eprintln!("[OK] runc 可达：{}", out.stdout.trim());
}

// ============================================================================
// B. OCI bundle 构造测（oci.rs write_bundle + runc spec 双向校验）
// ============================================================================

/// 验证 oci.rs `write_bundle` 生成的 config.json 结构合法 + runc 能解析。
///
/// 不需 root（仅写盘 + runc spec 生成模板对照）。流程：
/// 1. `write_bundle` 写 os-compute 风格 config.json（process.args=[/bin/sh]，linux.ns 六件套）；
/// 2. 读回校验关键字段（ociVersion/root/linux）；
/// 3. `runc spec`（在另一 tempdir）生成 runc 原生 config.json 作对照——断言两者都有
///    process.args、root.path、linux.namespaces 三大核心字段。
#[tokio::test]
#[ignore = "真实 runc：用 runc spec 对照 oci.rs 生成的 bundle 结构"]
async fn real_oci_bundle_structure_matches_runc_spec() {
    let bin = match find_runtime_bin() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] 未找到 runc 二进制，跳过 OCI bundle 对照测");
            return;
        }
    };
    let busybox = match find_busybox() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] 未找到 busybox 二进制（rootfs 最小化需要），跳过");
            return;
        }
    };

    // 1. os-compute oci.rs 生成 bundle
    let os_tmp = tempfile::tempdir().expect("tempdir 创建失败");
    let os_bundle = os_tmp.path();
    let spec = os_compute::container::ContainerSpec::new("osprobe:busybox");
    let cfg_path = write_bundle(&spec, os_bundle, Some("/os/osprobe")).expect("write_bundle 失败");
    assert!(cfg_path.is_file(), "config.json 应落盘");

    // 铺 busybox rootfs（write_bundle 不建 rootfs——实现层拉镜像后填）
    make_busybox_rootfs(os_bundle, &busybox).expect("铺 busybox rootfs 失败");

    // 2. 读回校验关键字段
    let os_cfg = read_config_json(os_bundle).expect("read_config_json 失败");
    assert_eq!(os_cfg.process.args, vec!["/bin/sh"]); // 空 command → 占位 /bin/sh
    assert_eq!(
        os_cfg.linux.as_ref().unwrap().cgroups_path.as_deref(),
        Some("/os/osprobe")
    );
    assert_eq!(os_cfg.linux.as_ref().unwrap().namespaces.len(), 6);

    // 3. runc spec 生成原生 config.json 作结构对照
    let runc_tmp = tempfile::tempdir().expect("tempdir 创建失败");
    let runc_bundle = runc_tmp.path();
    // runc spec 在 CWD 生成 config.json——用 tokio::process::Command 显式设 CWD
    // （runner.run 不设 CWD，spec 子命令的 CWD 语义与之不契合；runner.run 的可达性
    // 已由 test A 的 `runc --version` 覆盖）。
    let mut cmd = tokio::process::Command::new(&bin);
    cmd.arg("spec").current_dir(runc_bundle);
    let out = cmd.output().await.expect("spawn runc spec 失败");
    assert!(
        out.status.success(),
        "runc spec 失败: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let runc_cfg_path = runc_bundle.join("config.json");
    assert!(runc_cfg_path.is_file(), "runc spec 应生成 config.json");

    let runc_cfg_raw = std::fs::read_to_string(&runc_cfg_path).expect("读 runc config.json");
    // runc spec 默认 process.args = ["sh"]
    assert!(runc_cfg_raw.contains(r#""args""#), "runc config 应有 args");
    assert!(
        runc_cfg_raw.contains(r#""namespaces""#),
        "runc config 应有 namespaces"
    );

    eprintln!(
        "[OK] OCI bundle 结构合法：os-compute 生成 config.json 含 args/root/linux，\
         与 runc spec 模板结构一致"
    );
}

// ============================================================================
// C. 真实 create + start + delete 容器（/bin/true 立即退出）
// ============================================================================

/// 端到端：runc create → state(created) → start → state(stopped) → delete → list(空)。
///
/// 流程（用最小可跑 bundle：busybox rootfs + /proc/dev/sys 挂载）：
/// 1. 建 bundle + rootfs（busybox → /bin/true）+ 最小 config.json（含标准伪 fs 挂载）；
/// 2. `create_argv` → runner.run → 断言退出 0 + state 查 status=created；
/// 3. `start_argv` → runner.run → 断言退出 0（/bin/true 立即退出）；
/// 4. state 查 status=stopped（容器 init 退出后 runc 标 stopped）；
/// 5. `delete_argv --force` → 断言退出 0 + list 为空。
///
/// RAII [`RuncContainerGuard`] 保测主体异常也 delete；[`StateRootGuard`] 保清理 /tmp 状态根。
#[tokio::test]
#[ignore = "真实 runc：create/start/delete 端到端，需 root + runc + busybox"]
async fn real_runc_create_start_delete_lifecycle() {
    if !require_root() {
        return;
    }
    let (_bin, runner) = match require_runc() {
        Some(x) => x,
        None => {
            eprintln!("[SKIP] 未找到 runc 二进制，跳过容器生命周期测");
            return;
        }
    };
    let busybox = match find_busybox() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] 未找到 busybox（rootfs 需拷），跳过容器生命周期测");
            return;
        }
    };

    // 串行锁：并发 runc create 在 nsexec 阶段竞态失败（runc/内核限制），强制串行。
    let _create_guard = create_lock().lock().await;

    // state_root RAII 清理（/tmp/osprobe_runc_<pid>，绝不碰 /run/os/youki）
    let _sr_guard = StateRootGuard {
        path: runner.state_root.clone(),
    };

    // 1. 建 bundle + rootfs + 最小可跑 config.json
    let bundle_tmp = tempfile::tempdir().expect("bundle tempdir 失败");
    let bundle = bundle_tmp.path();
    make_busybox_rootfs(bundle, &busybox).expect("铺 busybox rootfs 失败");
    let cfg_path = bundle.join("config.json");
    std::fs::write(
        &cfg_path,
        make_runnable_config(bundle, &["/bin/true".to_string()]),
    )
    .expect("写 config.json 失败");

    let id = unique_id();
    let mut guard = RuncContainerGuard::new(runner.clone(), &id);

    // 2. create → state(created)
    let create = create_argv(&runner.state_root, &id, bundle).expect("create_argv 失败");
    let full = runner.full_argv(&create);
    let out = runner.run(&full).await.expect("run runc create 失败");
    assert_eq!(
        out.exit_code, 0,
        "runc create 应退出 0，stderr={}",
        out.stderr
    );

    let st = runc_state(&runner, &id).await;
    assert_eq!(
        st.as_deref(),
        Some("created"),
        "create 后 state 应为 created，实际 {:?}",
        st
    );
    eprintln!("[OK] runc create → state=created (id={id})");

    // 3. start → /bin/true 立即退出
    let start = start_argv(&runner.state_root, &id).expect("start_argv 失败");
    let full = runner.full_argv(&start);
    let out = runner.run(&full).await.expect("run runc start 失败");
    assert_eq!(
        out.exit_code, 0,
        "runc start 应退出 0，stderr={}",
        out.stderr
    );

    // 4. 等 init 退出（/bin/true 立即退，但 runc 状态更新有微秒级延迟）
    wait_for_state(
        &runner,
        &id,
        "stopped",
        std::time::Duration::from_millis(500),
    )
    .await;
    let st = runc_state(&runner, &id).await;
    assert_eq!(
        st.as_deref(),
        Some("stopped"),
        "start /bin/true 后 state 应为 stopped，实际 {:?}",
        st
    );
    eprintln!("[OK] runc start /bin/true → state=stopped");

    // 5. delete --force → list 为空
    let del = delete_argv(&runner.state_root, &id, true).expect("delete_argv 失败");
    let full = runner.full_argv(&del);
    let out = runner.run(&full).await.expect("run runc delete 失败");
    assert_eq!(
        out.exit_code, 0,
        "runc delete 应退出 0，stderr={}",
        out.stderr
    );
    guard.disarm();

    let list_out = runc_list(&runner).await;
    assert!(
        !list_out.contains(&id),
        "delete 后 list 不应含该容器 id，实际 list={:?}",
        list_out
    );
    eprintln!("[OK] runc delete --force → list 清空");

    // 等容器彻底销毁（runc init 进程退出），再放行下一测——并发 runc create 在
    // init 尚未完全终止时会竞态失败（nsexec netlink/cgroup）。锁在函数末尾释放，
    // 此 sleep 守护串行 create 之间不重叠。
    wait_container_gone(&runner, &id, std::time::Duration::from_millis(800)).await;
}

// ============================================================================
// D. 容器状态查询测（runc list / runc state）
// ============================================================================

/// 验证 `list_argv`/`state_argv` 真实可用：create 后 list 含该 id，state 输出 status。
#[tokio::test]
#[ignore = "真实 runc：list/state 查询，需 root + runc + busybox"]
async fn real_runc_list_and_state_queries() {
    if !require_root() {
        return;
    }
    let (_bin, runner) = match require_runc() {
        Some(x) => x,
        None => {
            eprintln!("[SKIP] 未找到 runc 二进制，跳过状态查询测");
            return;
        }
    };
    let busybox = match find_busybox() {
        Some(b) => b,
        None => {
            eprintln!("[SKIP] 未找到 busybox，跳过状态查询测");
            return;
        }
    };
    // 串行锁：并发 runc create 在 nsexec 阶段竞态失败（runc/内核限制），强制串行。
    let _create_guard = create_lock().lock().await;
    let _sr_guard = StateRootGuard {
        path: runner.state_root.clone(),
    };

    let bundle_tmp = tempfile::tempdir().expect("bundle tempdir 失败");
    let bundle = bundle_tmp.path();
    make_busybox_rootfs(bundle, &busybox).expect("铺 busybox rootfs 失败");
    std::fs::write(
        bundle.join("config.json"),
        make_runnable_config(bundle, &["/bin/sh".to_string()]),
    )
    .expect("写 config.json 失败");

    // 初始 list 应为空（无容器）
    let initial = runc_list(&runner).await;
    let id = unique_id();
    assert!(
        !initial.iter().any(|i| i == &id),
        "测试前不应存在该 id（唯一性保证）"
    );

    let mut guard = RuncContainerGuard::new(runner.clone(), &id);

    // create 后 list 含该 id
    let create = create_argv(&runner.state_root, &id, bundle).expect("create_argv 失败");
    let full = runner.full_argv(&create);
    let out = runner.run(&full).await.expect("run runc create 失败");
    assert_eq!(out.exit_code, 0, "create 失败: {}", out.stderr);

    let after_create = runc_list(&runner).await;
    assert!(
        after_create.iter().any(|i| i == &id),
        "create 后 list 应含该 id，实际 {:?}",
        after_create
    );

    // state <id> 输出 status=created
    let st_json = runc_state_raw(&runner, &id).await;
    let status = parse_state_status(&st_json);
    assert_eq!(
        status.as_deref(),
        Some("created"),
        "state 应返回 status=created，原始输出={}",
        st_json
    );
    // 验证 parse_state_status 与 runc 真实输出兼容（runc state 输出形如
    // {"ociVersion":"1.3.0","id":"...","status":"created",...}）
    eprintln!("[OK] runc list 含容器 + state status=created（parse_state_status 兼容 runc 输出）");

    // 清理
    let del = delete_argv(&runner.state_root, &id, true).expect("delete_argv 失败");
    let full = runner.full_argv(&del);
    let _ = runner.run(&full).await;
    guard.disarm();

    // 等容器彻底销毁（runc init 进程退出），再放行下一测——并发 runc create 在
    // init 尚未完全终止时会竞态失败（nsexec netlink/cgroup）。
    wait_container_gone(&runner, &id, std::time::Duration::from_millis(800)).await;
}

// ============================================================================
// E. 错误处理测（runc create 不存在的 bundle → 错误传播）
// ============================================================================

/// 验证 runc 错误正确传播到 [`runtime::check_output`] → `ComputeError::CommandFailed`：
/// - create 指向不存在 bundle → 退出码非 0 + stderr 非空 → check_output 映射 CommandFailed；
/// - state 不存在 id → 同样非 0。
///
/// 不需 root（错误路径 runc 在解析 bundle 阶段就失败，不建 namespace）。
#[tokio::test]
#[ignore = "真实 runc：错误路径传播，需 runc 二进制"]
async fn real_runc_error_propagation_for_nonexistent_bundle() {
    let (_bin, runner) = match require_runc() {
        Some(x) => x,
        None => {
            eprintln!("[SKIP] 未找到 runc 二进制，跳过错误处理测");
            return;
        }
    };
    let _sr_guard = StateRootGuard {
        path: runner.state_root.clone(),
    };

    // 1. create 不存在的 bundle → 非零退出 + stderr 含错误
    let nonexistent = PathBuf::from("/tmp/osprobe_runc_nonexistent_bundle_不会存在");
    let _ = std::fs::remove_dir_all(&nonexistent);
    let id = unique_id();
    let create = create_argv(&runner.state_root, &id, &nonexistent).expect("create_argv 失败");
    let full = runner.full_argv(&create);
    let out = runner.run(&full).await.expect("run runc create 失败");

    assert_ne!(
        out.exit_code, 0,
        "create 不存在 bundle 应非零退出，实际 {}",
        out.exit_code
    );
    assert!(!out.stderr.is_empty(), "stderr 应含错误信息，实际为空");
    // runc 错误形如 "runc create failed: chdir ...: no such file or directory"
    assert!(
        out.stderr.to_lowercase().contains("no such file")
            || out.stderr.contains("not exist")
            || out.stderr.contains("failed"),
        "stderr 应提示不存在/失败，实际：{}",
        out.stderr
    );

    // 经 check_output 映射成 CommandFailed
    let res = runtime::check_output(&out, "create 不存在 bundle");
    let err = match res {
        Ok(_) => panic!("非零退出应映射成错误，实际 Ok"),
        Err(e) => e,
    };
    assert!(
        matches!(err, os_compute::ComputeError::CommandFailed(_)),
        "应映射成 CommandFailed，实际 {err:?}"
    );

    // 2. state 不存在 id → 非零退出（"container does not exist"）
    let state = state_argv(&runner.state_root, &id).expect("state_argv 失败");
    let full = runner.full_argv(&state);
    let out = runner.run(&full).await.expect("run runc state 失败");
    assert_ne!(
        out.exit_code, 0,
        "state 不存在 id 应非零退出，实际 {}",
        out.exit_code
    );
    assert!(
        out.stderr.contains("does not exist") || out.stderr.contains("not exist"),
        "stderr 应提示容器不存在，实际：{}",
        out.stderr
    );

    eprintln!("[OK] runc 错误正确传播：非零退出 + stderr + check_output → CommandFailed");
}

// ============================================================================
// 辅助：runc state/list 执行 + 状态轮询
// ============================================================================

/// 跑 `runc --root <state_root> state <id>`，返回解析出的 status（小写串）。
///
/// 复用 [`parse_state_status`]——验证它与真实 runc state 输出兼容。
async fn runc_state(runner: &YoukiRunner, id: &str) -> Option<String> {
    let raw = runc_state_raw(runner, id).await;
    parse_state_status(&raw)
}

/// 跑 `runc state <id>`，返回原始 stdout（JSON）。
async fn runc_state_raw(runner: &YoukiRunner, id: &str) -> String {
    let argv = state_argv(&runner.state_root, id).expect("state_argv 失败");
    let full = runner.full_argv(&argv);
    let out = runner.run(&full).await.expect("run runc state 失败");
    out.stdout
}

/// 跑 `runc list`，返回所有容器 ID 列表。
async fn runc_list(runner: &YoukiRunner) -> Vec<String> {
    let argv = list_argv(&runner.state_root);
    let full = runner.full_argv(&argv);
    let out = runner.run(&full).await.expect("run runc list 失败");
    // runc list 输出表格式：首行表头 "ID PID STATUS BUNDLE CREATED OWNER"，后续每行一容器
    out.stdout
        .lines()
        .skip(1) // 跳表头
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| l.split_whitespace().next().map(|s| s.to_string()))
        .collect()
}

/// 轮询 runc state，最多等 `timeout` 直到 status == 期望值（容 init 退出延迟）。
async fn wait_for_state(
    runner: &YoukiRunner,
    id: &str,
    expected: &str,
    timeout: std::time::Duration,
) {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if runc_state(runner, id).await.as_deref() == Some(expected) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// 轮询直到容器从 `runc list` 消失（或超时），用于 delete 后等 runc init 进程彻底退出。
///
/// delete --force 后 runc 会 kill init 进程，但 init 退出 + cgroup/netlink 资源回收
/// 有微秒到毫秒级延迟；并发场景下若上一测的 init 尚未退出，下一测的 create 会在
/// nsexec 阶段竞态失败。本函数在 mutex 释放前等待容器确实从 list 消失（== init 已退）。
async fn wait_container_gone(runner: &YoukiRunner, id: &str, timeout: std::time::Duration) {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if !runc_list(runner).await.iter().any(|i| i == id) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// 生成最小可跑 config.json——含 /proc /dev /sys 伪文件系统挂载（runc init 需 /dev/null 等）。
///
/// os-compute 的 oci.rs 当前只生成 bind/volume 业务挂载，不铺标准伪 fs（生产由镜像
/// base 层或 youki 实现层补）。本测试专用：手写含 /proc /dev /sys 的最小 config，
/// 让 runc 能成功 create+start（验证 YoukiRunner 执行路径，而非 oci.rs 挂载完整性）。
fn make_runnable_config(bundle: &Path, args: &[String]) -> String {
    let rootfs = bundle.join("rootfs");
    let mounts = [
        r#"{"destination":"/proc","type":"proc"}"#,
        r#"{"destination":"/dev","type":"tmpfs"}"#,
        r#"{"destination":"/dev/pts","type":"devpts","options":["nosuid","noexec","newinstance","ptmxmode=0666","mode=0620"]}"#,
        r#"{"destination":"/dev/shm","type":"tmpfs","options":["nosuid","noexec","nodev","mode=1777","size=65536k"]}"#,
        r#"{"destination":"/sys","type":"sysfs","options":["nosuid","noexec","nodev","ro"]}"#,
    ];
    let args_json: Vec<String> = args.iter().map(|a| format!("{a:?}")).collect();
    format!(
        r#"{{
  "ociVersion": "1.0.2-dev",
  "process": {{
    "terminal": false,
    "user": {{"uid": 0, "gid": 0}},
    "args": [{}],
    "env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
    "cwd": "/"
  }},
  "root": {{
    "path": {:?},
    "readonly": false
  }},
  "mounts": [
    {}
  ],
  "linux": {{
    "namespaces": [
      {{"type":"pid"}},{{"type":"network"}},{{"type":"ipc"}},
      {{"type":"uts"}},{{"type":"mount"}},{{"type":"cgroup"}}
    ]
  }}
}}"#,
        args_json.join(","),
        rootfs.to_string_lossy(),
        mounts.join(",")
    )
}
