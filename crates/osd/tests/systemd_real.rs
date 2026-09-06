//! osd `SystemdOrchestrator` 真实 systemd 交互测（`#[ignore]`，手动跑）。
//!
//! ## 任务范围判断结论（写在最前，供后续集成者直接读）
//! 审查 `crates/osd/src/impl_orchestrator.rs` 后确认：
//! - `SystemdOrchestrator::{start, stop, restart}`（trait 实现，行 291-310）+ 内部
//!   `do_start_inner`/`do_stop_inner`（行 214-287）**是纯状态机框架**，无任何
//!   `systemctl` / `tokio::process::Command` 调用，也无 systemd unit 文件生成逻辑。
//!   行 245-250 / 282-285 明确 `TODO(集成阶段)`：检查依赖、生成 unit、`systemctl start`、
//!   健康探针轮询。
//! - 模块文档（行 7-18）同样明确：「真实 systemd 进程监管 / NTP 仍留 TODO」、
//!   「start/stop/restart：框架（状态转换可用，真实拉起进程待集成）」。
//!
//! 按红线「若 start/stop 实现是纯框架、接通 systemctl 会大改，只写『可达性 +
//! transient unit』测，不强改实现」，本文件**不修改 `SystemdOrchestrator` 实现**，
//! 只做三层真实回归：
//! 1. systemd 可达性（`systemctl --version` + `is-system-running`）；
//! 2. transient unit 完整生命周期（`systemd-run` 创建 → `is-active` 验证 → `stop` →
//!    清理），证明宿主 systemd 真能创建/查询/停止临时 unit —— 这是未来接通
//!    `SystemdOrchestrator` 真实路径的执行底座；
//! 3. `SystemdOrchestrator` 框架状态机在真实 systemd 主机上的内部一致性回归（start/
//!    stop/restart 状态转换、串行化、组件注册/拓扑），作为「真实环境跑不崩」基线，
//!    并显式断言「框架状态 ≠ systemctl 状态」（即 TODO 未接通的现状）。
//!
//! ## 运行环境
//! - systemd 主机（本机验证：systemd 259 / 259.5-0ubuntu3）；
//! - transient unit 写路径需 root（`systemd-run` 创建 unit 需特权）；非 root 优雅 SKIP；
//! - 测全部 `#[ignore]`，默认套件不跑；手动：
//!   `sudo cargo test -p osd --features mock --test systemd_real -- --ignored`
//!
//! ## 副作用红线（与任务红线一致）
//! - **只创建 `osd_test_` 前缀的 transient unit**（`systemd-run --unit=osd_test_*`）；
//! - **绝不**碰宿主真实服务、不改 `/etc/systemd/system/`；
//! - 每个 transient unit 用 RAII guard（`TransientUnit` Drop = `stop` + `reset-failed`）
//!   兜底清理，测前预清同名 unit（防上次残留）。

use std::process::Command;

use os_core::ResourceQuota;
use osd::component::HealthProbeConfig;
use osd::{
    ComponentDescriptor, ComponentId, ComponentRegistry, ComponentStatus, Orchestrator,
    SystemdOrchestrator,
};

/// 测试用 transient unit 前缀（与任务红线一致：仅创建此前缀的临时 unit）。
const UNIT_PREFIX: &str = "osd_test_";

// 注：root 提权由「sudo cargo test」在命令行完成（密码由跑测者输入），测试代码本身
// 在已是 root 的进程内直接调 `systemctl`/`systemd-run`，故此处不持有 sudo 密码。

/// 真实测要求 systemd 活着；无 systemd / 无 systemctl 命令时优雅 SKIP（不 panic）。
fn require_systemd_or_skip() {
    let probe = Command::new("systemctl").arg("--version").output();
    match probe {
        Ok(out) if out.status.success() => {} // systemd 在
        Ok(out) => {
            eprintln!(
                "[osd-systemd] SKIP: `systemctl --version` 非零退出 —— systemd 不可用。\n\
                 stderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!(
                "[osd-systemd] SKIP: 找不到 `systemctl` 命令 —— 无 systemd 环境（{}）。",
                e
            );
            std::process::exit(0);
        }
    }
}

/// 判断当前进程已是 root（uid 0）。sudo 跑测时为真，普通 cargo test 为假。
fn is_root() -> bool {
    // getuid 在 unix 可用；用 std::os::unix 风格不引入额外依赖
    #[cfg(unix)]
    {
        unsafe { libc_getuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

// 极简 getuid（避免为单点引入 libc 依赖：直接 extern syscall）。
#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

/// 需 root 才能跑的测的 SKIP 门（systemd-run 创建 transient unit 需特权）。
fn require_root_or_skip() {
    if is_root() {
        return;
    }
    eprintln!(
        "[osd-systemd] SKIP: 当前非 root —— `systemd-run` 创建 transient unit 需 root。\n\
         用 `sudo cargo test -p osd --features mock --test systemd_real -- --ignored` 跑。"
    );
    std::process::exit(0);
}

/// 生成唯一临时 unit 名（含 PID + 计数，避免并发测同名冲突）。
fn unique_unit_name(tag: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{UNIT_PREFIX}{tag}_{}_{}", std::process::id(), n)
}

/// RAII guard：持有一个 transient unit，Drop 时尽力清理（stop + reset-failed）。
///
/// 即使测中途 panic，Drop 也会跑，确保无残留 unit 污染宿主 systemd。
struct TransientUnit {
    name: String,
    cleaned: bool,
}

impl TransientUnit {
    /// 创建 guard（不创建 unit 本身——unit 由调用方用 systemd-run 创建后 `from_name`）。
    fn guard_for(name: String) -> Self {
        Self {
            name,
            cleaned: false,
        }
    }

    /// 兜底清理：`systemctl stop`（幂等）+ `systemctl reset-failed`（幂等）。
    /// 不报错——清理失败只打 stderr（已删/未加载属正常）。
    fn cleanup(&mut self) {
        // stop 幂等：unit 已 inactive/未加载时 rc 非 0，属正常
        let _ = Command::new("systemctl")
            .args(["stop", &self.name])
            .status();
        // reset-failed 幂等：unit 已卸载时报 "not loaded"，属正常
        let _ = Command::new("systemctl")
            .args(["reset-failed", &self.name])
            .status();
        self.cleaned = true;
    }
}

impl Drop for TransientUnit {
    fn drop(&mut self) {
        if !self.cleaned {
            self.cleanup();
        }
    }
}

/// 跑一个 `systemctl …` 命令并返回 (成功?, stdout, stderr)。
fn run_systemctl(args: &[&str]) -> (bool, String, String) {
    let out = Command::new("systemctl").args(args).output();
    match out {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).trim().to_string(),
            String::from_utf8_lossy(&o.stderr).trim().to_string(),
        ),
        Err(e) => (false, String::new(), format!("spawn systemctl 失败: {e}")),
    }
}

/// `systemd-run --unit=<name> …` 创建 transient unit，返回成功? + stderr。
fn systemd_run(args: &[&str]) -> (bool, String) {
    let out = Command::new("systemd-run").args(args).output();
    match out {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stderr).trim().to_string(),
        ),
        Err(e) => (false, format!("spawn systemd-run 失败: {e}")),
    }
}

// =============================================================================
// 测 1：systemd 可达性
// =============================================================================

/// 验证 systemd 活着：`systemctl --version` 成功 + 版本号 ≥ 200（极宽松下界，防 parse 复杂）；
/// `systemctl is-system-running` 返回任意已知状态（running/degraded/maintenance/starting/
/// stopped/offline/unknown）—— degraded 也算「systemd 活着」（本机实测即为 degraded）。
#[test]
#[ignore = "真实 systemd 可达性：手动 `cargo test -p osd --features mock --test systemd_real -- --ignored systemd_reachable`（非 root 可跑）"]
fn systemd_reachable() {
    require_systemd_or_skip();

    // 1. systemctl --version
    let (ok, stdout, stderr) = run_systemctl(&["--version"]);
    assert!(ok, "`systemctl --version` 应成功。stderr: {stderr}");
    let first_line = stdout.lines().next().unwrap_or("");
    eprintln!("[osd-systemd] systemctl --version 首行: {first_line}");
    // 首行形如 "systemd 259 (259.5-0ubuntu3)"
    let ver_token = first_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse::<u32>()
        .unwrap_or(0);
    assert!(
        ver_token >= 200,
        "systemd 主版本号 ≥ 200 期望，实际解析: {ver_token}（首行: {first_line}）"
    );

    // 2. is-system-running（degraded 也算活着——本机实测即 degraded）
    let (ok, stdout, _stderr) = run_systemctl(&["is-system-running"]);
    eprintln!("[osd-systemd] systemctl is-system-running → rc={ok}, state={stdout:?}");
    let known_states = [
        "initializing",
        "starting",
        "running",
        "degraded",
        "maintenance",
        "stopping",
    ];
    // rc 非 0 但 stdout 是已知状态（如 degraded rc=1）也算 systemd 活着
    let known = known_states.iter().any(|s| stdout == *s);
    assert!(
        ok || known,
        "systemd 应处于已知运行状态。state={stdout:?}（rc={ok}）"
    );
}

// =============================================================================
// 测 2：transient unit 完整生命周期（oneshot + RemainAfterExit）
// =============================================================================

/// 验证 `systemd-run` 能创建 transient unit，且 `systemctl is-active`/`stop`/`reset-failed`
/// 完整生命周期在本机真实 systemd 259 上跑通。
///
/// 用 `--service-type=oneshot --remain`（RemainAfterExit=yes）：oneshot 跑完 /bin/true 后
/// unit 留在 active 状态，便于验证 start/stop 语义（这是 transient unit 做「服务生命周期
/// 探针」的标准形态，也是未来 `SystemdOrchestrator::start` 真实路径的执行底座）。
#[test]
#[ignore = "真实 transient unit 生命周期：手动 `sudo cargo test -p osd --features mock --test systemd_real -- --ignored transient_unit_lifecycle`（需 root）"]
fn transient_unit_lifecycle() {
    require_systemd_or_skip();
    require_root_or_skip();

    let unit = unique_unit_name("lc");
    // 测前预清（防上次残留）
    let mut guard = TransientUnit::guard_for(unit.clone());
    guard.cleanup();
    // guard.cleanup 后 cleaned=true，但本测后面还要用它兜底，重置标志
    guard.cleaned = false;

    eprintln!("[osd-systemd] 创建 transient unit: {unit}");
    let (ok, stderr) = systemd_run(&[
        "--unit",
        &unit,
        "--service-type=oneshot",
        "--remain",
        "/bin/true",
    ]);
    assert!(ok, "systemd-run 创建 unit 失败。stderr: {stderr}");

    // 略等 systemd 完成 oneshot + 转 active
    std::thread::sleep(std::time::Duration::from_millis(300));

    // is-active 应为 "active"（RemainAfterExit）
    let (ok, stdout, _stderr) = run_systemctl(&["is-active", &unit]);
    assert!(ok, "unit 应 active。is-active 输出: {stdout:?}");
    assert_eq!(
        stdout, "active",
        "RemainAfterExit 下 oneshot 完成后 unit 应为 active"
    );

    // systemctl stop → rc=0
    let (ok, _stdout, stderr) = run_systemctl(&["stop", &unit]);
    assert!(ok, "stop 应成功。stderr: {stderr}");

    // stop 后 is-active 应为 "inactive"
    let (ok, stdout, _stderr) = run_systemctl(&["is-active", &unit]);
    // is-active 在 inactive 时 rc=3（非 0），故不能断言 ok；只看 stdout
    assert_eq!(
        stdout, "inactive",
        "stop 后 unit 应为 inactive（is-active rc={ok} 在 inactive 时为 3，正常）"
    );

    // 清理（reset-failed 幂等；stop 后 unit 多已自动卸载，reset-failed 报 "not loaded" 属正常）
    guard.cleanup();
    let (ok, stdout, _stderr) = run_systemctl(&["is-active", &unit]);
    let gone = stdout == "inactive" || stdout == "unknown";
    assert!(
        gone,
        "清理后 unit 应不可达（inactive/unknown）。is-active: {stdout:?} (rc={ok})"
    );

    eprintln!("[osd-systemd] transient unit {unit} 生命周期完整跑通 ✓");
}

// =============================================================================
// 测 3：transient unit 长跑进程 start/stop（exec 类型，验证真实进程监管语义）
// =============================================================================

/// 验证 `systemd-run` 创建一个**长跑** transient unit（`--service-type=exec` + sleep 进程），
/// 然后 `systemctl is-active` 确认进程在跑，`systemctl stop` 优雅终止，确认变 inactive。
///
/// 这是 transient unit 作为「真实守护进程替身」的标准形态——比测 2 的 oneshot 更贴近
/// `SystemdOrchestrator` 未来要监管的「长期运行组件」。证明宿主 systemd 能真实拉起 +
/// 监管 + 终止一个长期进程（含 SIGTERM 优雅退出语义）。
#[test]
#[ignore = "真实 transient unit 长跑进程监管：手动 `sudo cargo test -p osd --features mock --test systemd_real -- --ignored transient_unit_long_running`（需 root）"]
fn transient_unit_long_running() {
    require_systemd_or_skip();
    require_root_or_skip();

    let unit = unique_unit_name("lr");
    let mut guard = TransientUnit::guard_for(unit.clone());
    guard.cleanup();
    guard.cleaned = false;

    eprintln!("[osd-systemd] 创建长跑 transient unit: {unit}");
    // sleep 60：足够测完前进程一直在；测后 stop 会提前 SIGTERM 它
    let (ok, stderr) = systemd_run(&["--unit", &unit, "--service-type=exec", "/bin/sleep", "60"]);
    assert!(ok, "systemd-run 创建长跑 unit 失败。stderr: {stderr}");

    // 略等 systemd 完成 exec 启动（exec 类型需等 execve 成功才转 active）
    std::thread::sleep(std::time::Duration::from_millis(400));

    let (ok, stdout, _stderr) = run_systemctl(&["is-active", &unit]);
    assert!(ok, "长跑 unit 应 active。is-active: {stdout:?}");
    assert_eq!(stdout, "active", "长跑进程应在运行");

    // stop → systemd 发 SIGTERM，sleep 60 会优雅退出
    let (ok, _stdout, stderr) = run_systemctl(&["stop", &unit]);
    assert!(ok, "stop 长跑 unit 应成功。stderr: {stderr}");

    let (ok, stdout, _stderr) = run_systemctl(&["is-active", &unit]);
    assert_eq!(
        stdout, "inactive",
        "stop 后长跑 unit 应为 inactive（is-active rc={ok}）"
    );

    guard.cleanup();
    eprintln!("[osd-systemd] 长跑 transient unit {unit} 监管语义跑通 ✓");
}

// =============================================================================
// 测 4：SystemdOrchestrator 框架状态机在真实 systemd 主机上的内部一致性
// =============================================================================

/// 在真实 systemd 主机（本机 systemd 259）上回归 `SystemdOrchestrator` 的**框架逻辑**：
/// 组件注册、拓扑启动顺序、start/stop/restart 状态转换、同组件串行化、set_quota/get_quota。
///
/// **架构锚点（do_start_inner/do_stop_inner 已接通 SystemdRunner trait）**：本测用默认
/// `InMemorySystemdRunner`（no-op）构造，验证「no-op runner 不创建真实 unit，但框架状态机
/// 仍正确转 Running/Stopped」—— 即接通后的向后兼容性（非 root、纯内存）。真实 systemctl
/// 接通（注入 `TokioSystemdRunner`，断言「框架状态 == systemctl is-active」）见
/// `systemd_integrate_real.rs`。
#[tokio::test]
#[ignore = "SystemdOrchestrator 框架状态机（no-op runner）+ 架构锚点：手动 `cargo test -p osd --features mock --test systemd_real -- --ignored orchestrator_state_machine`（非 root 可跑，no-op runner 不碰 systemd）"]
async fn orchestrator_state_machine() {
    require_systemd_or_skip();
    // 框架逻辑不写 systemd，无需 root；但仍要求 systemd 活着（本测的语义前提）

    // 构造编排器：注入内存 cgroup 后端（避免真写 /sys/fs/cgroup，框架测焦点在状态机）
    use osd::InMemoryCgroupBackend;
    let mk = |id: &str, deps: &[&str]| ComponentDescriptor {
        id: ComponentId::new(id),
        dependencies: deps.iter().map(|&s| ComponentId::new(s)).collect(),
        quota: ResourceQuota {
            cpu_cores: Some(1.0),
            memory_bytes: None,
            io_bps_limit: None,
        },
        health_probe: HealthProbeConfig {
            kind: "exec".into(),
            target: "/bin/true".into(),
            interval_secs: 10,
            timeout_secs: 1,
            failure_threshold: 3,
        },
        command: Some("/bin/true".into()),
        enabled: true,
    };

    let registry = ComponentRegistry::from_descriptors(vec![
        mk("base", &[]),
        mk("svc_a", &["base"]),
        mk("svc_b", &["base"]),
    ]);
    let orch = SystemdOrchestrator::with_cgroup_backend(
        registry,
        "os",
        Box::new(InMemoryCgroupBackend::new()),
    );

    // 1. 拓扑序：base 应在 svc_a / svc_b 之前
    let order = orch.startup_order().expect("拓扑序应算出");
    let pos = |id: &str| order.iter().position(|x| x.as_str() == id).unwrap();
    assert!(pos("base") < pos("svc_a"));
    assert!(pos("base") < pos("svc_b"));
    eprintln!("[osd-systemd] 拓扑启动顺序: {:?}", order);

    // 2. 初始状态全 Stopped
    for &id in &["base", "svc_a", "svc_b"] {
        assert_eq!(
            orch.status(&ComponentId::new(id)).await.unwrap(),
            ComponentStatus::Stopped,
            "{id} 初始应 Stopped"
        );
    }

    // 3. start base → Running（框架：直接状态转换，不真拉进程）
    orch.start(&ComponentId::new("base")).await.unwrap();
    assert_eq!(
        orch.status(&ComponentId::new("base")).await.unwrap(),
        ComponentStatus::Running
    );
    // 幂等：再 start 不报错
    orch.start(&ComponentId::new("base")).await.unwrap();

    // 4. start 依赖未就绪的 svc_a 也允许（框架当前不校验依赖 Running，TODO 行 246）
    orch.start(&ComponentId::new("svc_a")).await.unwrap();
    assert_eq!(
        orch.status(&ComponentId::new("svc_a")).await.unwrap(),
        ComponentStatus::Running
    );

    // 5. restart svc_a → 仍 Running
    orch.restart(&ComponentId::new("svc_a")).await.unwrap();
    assert_eq!(
        orch.status(&ComponentId::new("svc_a")).await.unwrap(),
        ComponentStatus::Running
    );

    // 6. stop svc_a → Stopped；幂等
    orch.stop(&ComponentId::new("svc_a")).await.unwrap();
    assert_eq!(
        orch.status(&ComponentId::new("svc_a")).await.unwrap(),
        ComponentStatus::Stopped
    );
    orch.stop(&ComponentId::new("svc_a")).await.unwrap();

    // 7. set_quota/get_quota 经内存后端往返
    let q = ResourceQuota {
        cpu_cores: Some(2.5),
        memory_bytes: Some(512 * 1024 * 1024),
        io_bps_limit: None,
    };
    orch.set_quota(&ComponentId::new("base"), q).await.unwrap();
    let got = orch.get_quota(&ComponentId::new("base")).await.unwrap();
    assert_eq!(got.cpu_cores, Some(2.5));
    assert_eq!(got.memory_bytes, Some(512 * 1024 * 1024));

    // 8. list_components 返回全部已注册
    let list = orch.list_components().await.unwrap();
    assert_eq!(list.len(), 3);

    // 9. **架构锚点（do_start_inner/do_stop_inner 已接通 SystemdRunner trait）**：
    //    本测用默认 `InMemorySystemdRunner`（no-op）构造——它不创建任何真实 systemd unit，
    //    仅在内存里记 active/inactive 状态，让框架状态机跑通（非 root 可跑，不碰宿主）。
    //    故宿主 systemd 不应有 active 的同名 unit（`is-active` 返回 inactive/unknown）。
    //    **真实 systemctl 路径（注入 `TokioSystemdRunner`）的验证见 `systemd_integrate_real.rs`**
    //    —— 那个测断言「框架状态 == systemctl is-active」（start→双 active，stop→双 inactive）。
    //    两个测互补：本测覆盖框架逻辑（非 root），那个测覆盖真实接通（root）。
    for &id in &["base", "svc_a", "svc_b"] {
        let unit_name = format!("osd-{id}.service"); // 默认 InMemorySystemdRunner 的 unit 命名约定
        let (ok, stdout, _stderr) = run_systemctl(&["is-active", &unit_name]);
        // is-active rc 非 0（inactive rc=3 / unknown rc=?），stdout 非 active
        assert_ne!(
            stdout, "active",
            "no-op runner 不创建真实 unit，宿主不应有 active 的 osd-{id} unit（rc={ok}）"
        );
    }
    eprintln!(
        "[osd-systemd] 架构锚点确认：默认 InMemorySystemdRunner（no-op）不创建真实 unit；\
        真实 systemctl 接通验证见 systemd_integrate_real.rs（注入 TokioSystemdRunner）"
    );

    // 10. 同组件串行化（框架逻辑不 panic）
    use std::sync::Arc;
    let orch = Arc::new(orch);
    let mut handles = vec![];
    for _ in 0..8 {
        let o = orch.clone();
        handles.push(tokio::spawn(async move {
            o.start(&ComponentId::new("svc_b")).await
        }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }
    assert_eq!(
        orch.status(&ComponentId::new("svc_b")).await.unwrap(),
        ComponentStatus::Running
    );

    eprintln!("[osd-systemd] SystemdOrchestrator 框架状态机在真实 systemd 主机上回归通过 ✓");
}
