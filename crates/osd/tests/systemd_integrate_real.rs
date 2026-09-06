//! osd `SystemdOrchestrator` 真实接通测（`do_start_inner`/`do_stop_inner` → 真实 systemctl）。
//!
//! ## 任务范围
//! batch4（`systemd_real.rs`）验证了「systemd 可达性 + transient unit 生命周期」底座，
//! 并锚点断言「框架状态机 ≠ systemd 真实 unit」（即 `do_start_inner`/`do_stop_inner` 仍是纯框架）。
//! 本文件验证**接通后的真实路径**：`SystemdOrchestrator` 注入 [`TokioSystemdRunner`]，
//! `start()` 真的调 `systemd-run` 创建 transient unit + `is-active` 轮询确认 active，
//! `stop()` 真的调 `systemctl stop` 终止进程 + `reset-failed` 清理。
//!
//! ## 运行环境
//! - systemd 主机（本机验证：systemd 259）；
//! - **需 root**（`systemd-run` 创建 transient unit 需特权）；非 root 优雅 SKIP；
//! - 测全部 `#[ignore]`，默认套件不跑；手动：
//!   `sudo cargo test -p osd --features mock --test systemd_integrate_real -- --ignored --nocapture`
//!
//! ## 副作用红线（与任务红线一致）
//! - **只创建唯一 `osd-test-<pid>-<nanos>-` 前缀的 transient unit**（避免碰宿主真实服务）；
//! - 每个 unit 用 RAII guard（Drop = `stop` + `reset-failed`）兜底清理；
//! - 测前预清同名 unit（防上次残留）；
//! - 组件 ExecStart 用 `/bin/sleep 60`（长跑占位，测完 stop 会提前 SIGTERM）。
//!
//! ## 与 batch4 测的关系
//! - batch4 `orchestrator_state_machine`：用 **默认 `InMemorySystemdRunner`（no-op）** 跑框架
//!   状态机，非 root 可跑，验证「no-op runner 不创建 unit」（注释说明真实路径见本文件）。
//! - 本文件：注入 **`TokioSystemdRunner`（真实）**，root 跑，验证「状态机 == systemctl 真实状态」。

use std::process::Command;

use os_core::ResourceQuota;
use osd::component::HealthProbeConfig;
use osd::{
    ComponentDescriptor, ComponentId, ComponentRegistry, ComponentStatus, Orchestrator,
    SystemdOrchestrator, SystemdRunner, TokioSystemdRunner,
};

// =============================================================================
// SKIP 门 + 辅助（与 systemd_real.rs 同款，保持一致）
// =============================================================================

/// 真实测要求 systemd 活着；无 systemd / 无 systemctl 命令时优雅 SKIP（不 panic）。
fn require_systemd_or_skip() {
    let probe = Command::new("systemctl").arg("--version").output();
    match probe {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            eprintln!(
                "[osd-integrate] SKIP: `systemctl --version` 非零退出 —— systemd 不可用。\n\
                 stderr: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!(
                "[osd-integrate] SKIP: 找不到 `systemctl` 命令 —— 无 systemd 环境（{}）。",
                e
            );
            std::process::exit(0);
        }
    }
}

/// 判断当前进程已是 root（uid 0）。
fn is_root() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc_getuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(unix)]
unsafe extern "C" {
    #[link_name = "getuid"]
    fn libc_getuid() -> u32;
}

/// 需 root 才能跑的测的 SKIP 门。
fn require_root_or_skip() {
    if is_root() {
        return;
    }
    eprintln!(
        "[osd-integrate] SKIP: 当前非 root —— `systemd-run` 创建 transient unit 需 root。\n\
         用 `sudo cargo test -p osd --features mock --test systemd_integrate_real -- --ignored --nocapture` 跑。"
    );
    std::process::exit(0);
}

/// 生成唯一 unit 前缀（PID + 纳秒 + 计数），确保不碰宿主真实服务 + 测间不冲突。
///
/// 返回的前缀会被 `TokioSystemdRunner::with_prefix` 使用，最终 unit 名形如
/// `osd-test-<pid>-<nanos>-<id>`。
fn unique_unit_prefix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "osd-test-{}-{}-{}-",
        std::process::id(),
        nanos % 1_000_000_000,
        n
    )
}

/// RAII guard：持有一个 unit 名，Drop 时尽力清理（stop + reset-failed）。
struct UnitGuard {
    name: String,
}

impl UnitGuard {
    fn new(name: String) -> Self {
        let g = Self { name };
        // 测前预清（防上次残留）
        g.cleanup();
        g
    }
    fn cleanup(&self) {
        let _ = Command::new("systemctl")
            .args(["stop", &self.name])
            .status();
        let _ = Command::new("systemctl")
            .args(["reset-failed", &self.name])
            .status();
    }
}

impl Drop for UnitGuard {
    fn drop(&mut self) {
        self.cleanup();
    }
}

/// 跑 `systemctl …` 返回 stdout（trim）；命令本身 spawn 失败才 panic。
fn systemctl_stdout(args: &[&str]) -> String {
    let out = Command::new("systemctl")
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("spawn systemctl 失败: {e}"));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// 构造一个测试用 ComponentDescriptor（ExecStart = /bin/sleep 60 长跑占位）。
fn make_desc(id: &str, deps: &[&str], command: Option<&str>) -> ComponentDescriptor {
    ComponentDescriptor {
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
        command: command.map(|s| s.to_string()),
        enabled: true,
    }
}

/// 构造注入真实 TokioSystemdRunner 的编排器（cgroup 用内存后端避免真写 /sys/fs/cgroup）。
///
/// `unit_prefix` 经 `TokioSystemdRunner::with_prefix` 注入，runner 的 `unit_name_for`
/// 会产出 `{unit_prefix}{component_id}`（如 `osd-test-123-456-0-svc`）—— 这就是宿主
/// systemd 真实 unit 名。返回 `id_to_unit` 映射供测试做 RAII guard + is-active 探测。
fn build_real_orchestrator(
    descs: Vec<ComponentDescriptor>,
    unit_prefix: &str,
) -> (SystemdOrchestrator, Vec<(ComponentId, String)>) {
    let runner = TokioSystemdRunner::with_prefix(unit_prefix);
    // 预算每个组件的 unit 名（与 runner.unit_name_for 一致）
    let id_to_unit: Vec<(ComponentId, String)> = descs
        .iter()
        .map(|d| {
            let unit = runner.unit_name_for(&d.id);
            (d.id.clone(), unit)
        })
        .collect();
    let registry = ComponentRegistry::from_descriptors(descs);
    let orch = SystemdOrchestrator::with_systemd_runner(registry, Box::new(runner));
    (orch, id_to_unit)
}

// =============================================================================
// 测 a：do_start_inner 真实拉起（systemd-run → is-active active）
// =============================================================================

/// 验证 `start()` 经 `TokioSystemdRunner` 真的调 `systemd-run` 创建 transient unit，
/// 且宿主 systemd 真有同名 active unit（`systemctl is-active` == active）。
///
/// ExecStart = `/bin/sleep 60`（长跑 exec 类型，测完 stop 提前终止）。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "真实 do_start 接通：手动 `sudo cargo test -p osd --features mock --test systemd_integrate_real -- --ignored start_real_pulls_up_unit`（需 root）"]
async fn start_real_pulls_up_unit() {
    require_systemd_or_skip();
    require_root_or_skip();

    let prefix = unique_unit_prefix();
    let (orch, id_to_unit) =
        build_real_orchestrator(vec![make_desc("svc", &[], Some("/bin/sleep 60"))], &prefix);
    let cid = &id_to_unit[0].0;
    let unit = &id_to_unit[0].1;
    let _guard = UnitGuard::new(unit.clone());

    eprintln!("[osd-integrate] start 组件 {cid}（unit={unit}）");
    orch.start(cid).await.expect("start 应成功");

    // 1. 编排器状态机 == Running
    let st = orch.status(cid).await.unwrap();
    assert_eq!(
        st,
        ComponentStatus::Running,
        "start 后编排器状态应为 Running"
    );

    // 2. 宿主 systemd 真有同名 active unit
    let active = systemctl_stdout(&["is-active", unit]);
    assert_eq!(
        active, "active",
        "宿主 systemd 应有 active 的 unit {unit}（实际: {active}）"
    );

    eprintln!("[osd-integrate] start 真实拉起 ✓ unit={unit} is-active=active");
}

// =============================================================================
// 测 b：do_stop_inner 真实停止（systemctl stop → is-active inactive）
// =============================================================================

/// 验证 `stop()` 经 `TokioSystemdRunner` 真的调 `systemctl stop` 终止进程，
/// 且宿主 systemd `is-active` == inactive。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "真实 do_stop 接通：手动 `sudo cargo test -p osd --features mock --test systemd_integrate_real -- --ignored stop_real_terminates_unit`（需 root）"]
async fn stop_real_terminates_unit() {
    require_systemd_or_skip();
    require_root_or_skip();

    let prefix = unique_unit_prefix();
    let (orch, id_to_unit) =
        build_real_orchestrator(vec![make_desc("svc", &[], Some("/bin/sleep 60"))], &prefix);
    let cid = &id_to_unit[0].0;
    let unit = &id_to_unit[0].1;
    let _guard = UnitGuard::new(unit.clone());

    // 先 start
    orch.start(cid).await.unwrap();
    assert_eq!(
        systemctl_stdout(&["is-active", unit]),
        "active",
        "前置：start 后应 active"
    );

    // stop
    eprintln!("[osd-integrate] stop 组件 {cid}（unit={unit}）");
    orch.stop(cid).await.expect("stop 应成功");

    // 1. 编排器状态机 == Stopped
    let st = orch.status(cid).await.unwrap();
    assert_eq!(
        st,
        ComponentStatus::Stopped,
        "stop 后编排器状态应为 Stopped"
    );

    // 2. 宿主 systemd is-active == inactive
    let active = systemctl_stdout(&["is-active", unit]);
    assert!(
        active == "inactive" || active == "unknown" || active == "dead",
        "stop 后 unit 应 inactive/unknown/dead（实际: {active}）"
    );

    eprintln!("[osd-integrate] stop 真实终止 ✓ unit={unit} is-active={active}");
}

// =============================================================================
// 测 c：状态机一致性（start 后双 Running/active，stop 后双 Stopped/inactive）
// =============================================================================

/// 验证「编排器状态机状态」与「systemctl 真实状态」在 start/stop 两端都一致。
///
/// 这是接通后的核心保证：状态机不再「自说自话」，而是真实反映宿主 systemd 状态。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "真实状态机一致性：手动 `sudo cargo test -p osd --features mock --test systemd_integrate_real -- --ignored state_machine_matches_systemctl`（需 root）"]
async fn state_machine_matches_systemctl() {
    require_systemd_or_skip();
    require_root_or_skip();

    let prefix = unique_unit_prefix();
    let (orch, id_to_unit) =
        build_real_orchestrator(vec![make_desc("svc", &[], Some("/bin/sleep 60"))], &prefix);
    let cid = &id_to_unit[0].0;
    let unit = &id_to_unit[0].1;
    let _guard = UnitGuard::new(unit.clone());

    // 初始：编排器 Stopped（默认），systemctl 非 active
    assert_eq!(
        orch.status(cid).await.unwrap(),
        ComponentStatus::Stopped,
        "初始应 Stopped"
    );
    let before = systemctl_stdout(&["is-active", unit]);
    assert_ne!(
        before, "active",
        "初始宿主不应有 active unit（实际: {before}）"
    );

    // start → 双 active
    orch.start(cid).await.unwrap();
    assert_eq!(
        orch.status(cid).await.unwrap(),
        ComponentStatus::Running,
        "start 后编排器 Running"
    );
    assert_eq!(
        systemctl_stdout(&["is-active", unit]),
        "active",
        "start 后宿主 active"
    );

    // stop → 双 inactive
    orch.stop(cid).await.unwrap();
    assert_eq!(
        orch.status(cid).await.unwrap(),
        ComponentStatus::Stopped,
        "stop 后编排器 Stopped"
    );
    let after = systemctl_stdout(&["is-active", unit]);
    assert!(
        after == "inactive" || after == "unknown" || after == "dead",
        "stop 后宿主 inactive/unknown/dead（实际: {after}）"
    );

    eprintln!("[osd-integrate] 状态机一致性 ✓ start→(Running,active) stop→(Stopped,inactive)");
}

// =============================================================================
// 测 d：start → stop → start 重启可重复
// =============================================================================

/// 验证 start→stop→start 循环可重复（不残留、不卡死），每次两端状态都一致。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "真实重启循环：手动 `sudo cargo test -p osd --features mock --test systemd_integrate_real -- --ignored restart_cycle_repeatable`（需 root）"]
async fn restart_cycle_repeatable() {
    require_systemd_or_skip();
    require_root_or_skip();

    let prefix = unique_unit_prefix();
    let (orch, id_to_unit) =
        build_real_orchestrator(vec![make_desc("svc", &[], Some("/bin/sleep 60"))], &prefix);
    let cid = &id_to_unit[0].0;
    let unit = &id_to_unit[0].1;
    let _guard = UnitGuard::new(unit.clone());

    for round in 1..=2 {
        eprintln!("[osd-integrate] 重启循环 round {round}");
        // start
        orch.start(cid).await.expect("round {round} start 应成功");
        assert_eq!(
            orch.status(cid).await.unwrap(),
            ComponentStatus::Running,
            "round {round} start 后 Running"
        );
        assert_eq!(
            systemctl_stdout(&["is-active", unit]),
            "active",
            "round {round} start 后宿主 active"
        );
        // stop
        orch.stop(cid).await.expect("round {round} stop 应成功");
        assert_eq!(
            orch.status(cid).await.unwrap(),
            ComponentStatus::Stopped,
            "round {round} stop 后 Stopped"
        );
        let after = systemctl_stdout(&["is-active", unit]);
        assert!(
            after == "inactive" || after == "unknown" || after == "dead",
            "round {round} stop 后宿主 inactive/unknown/dead（实际: {after}）"
        );
    }

    eprintln!("[osd-integrate] 重启循环可重复 ✓ unit={unit}");
}

// =============================================================================
// 测 e（bonus）：do_stop_inner 对从未 start 过的组件也安全（幂等清理）
// =============================================================================

/// 验证对一个从未 start 过的组件直接 stop（状态 None），编排器返回 Ok 且不 panic，
/// 且宿主无残留 unit（do_stop_inner 的兜底 stop_unit 调用幂等）。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "真实 stop 幂等清理：手动 `sudo cargo test -p osd --features mock --test systemd_integrate_real -- --ignored stop_idempotent_on_never_started`（需 root）"]
async fn stop_idempotent_on_never_started() {
    require_systemd_or_skip();
    require_root_or_skip();

    let prefix = unique_unit_prefix();
    let (orch, id_to_unit) =
        build_real_orchestrator(vec![make_desc("svc", &[], Some("/bin/sleep 60"))], &prefix);
    let cid = &id_to_unit[0].0;
    let unit = &id_to_unit[0].1;
    let _guard = UnitGuard::new(unit.clone());

    // 从未 start，直接 stop：应 Ok
    orch.stop(cid).await.expect("从未 start 的组件 stop 应 Ok");
    assert_eq!(
        orch.status(cid).await.unwrap(),
        ComponentStatus::Stopped,
        "stop 后应 Stopped"
    );
    let active = systemctl_stdout(&["is-active", unit]);
    assert!(
        active == "inactive" || active == "unknown" || active == "dead",
        "从未 start 的 unit stop 后应 inactive/unknown/dead（实际: {active}）"
    );

    eprintln!("[osd-integrate] stop 幂等清理 ✓ 从未 start 的组件 stop 安全");
}
