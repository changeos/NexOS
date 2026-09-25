//! `SystemdRunner` —— systemd 进程监管的后端抽象
//!
//! 定位（规格书 §3 关键实现 / §9 红线）：[`crate::SystemdOrchestrator`] 的
//! `do_start_inner` / `do_stop_inner` 需要真实 `systemctl` / `systemd-run` 调用
//! 来拉起 / 停止 transient unit。本模块把所有 systemd 命令交互抽象成 trait，
//! 与 [`crate::CgroupBackend`] / [`crate::ntp_impl::NtpRunner`] 风格一致：
//!
//! | 后端 | 用途 | 真跑 systemctl？ |
//! |------|------|-----------------|
//! | [`TokioSystemdRunner`] | 生产 + 真实集成测（root + systemd） | ✅ `tokio::process::Command` 跑 `systemctl`/`systemd-run` |
//! | [`InMemorySystemdRunner`] | 单元测试 / framework 锚点测 | ❌ 仅记录调用 + 返回预设 is_active |
//!
//! [`SystemdOrchestrator`](crate::SystemdOrchestrator) **默认注入 [`InMemorySystemdRunner`]**
//! （no-op），保持现有单元测与 batch4 框架锚点测的非 root、纯状态机语义；
//! 真实集成测（`tests/systemd_integrate_real.rs`）通过 `with_systemd_runner`
//! 构造函数注入 [`TokioSystemdRunner`]，真正在宿主 systemd 上拉起 transient unit。
//!
//! ## 权限与红线（规格书 §6 硬阻塞 / §9）
//! 真实 systemd 操作需 **root + systemd（PID1 为 systemd）+ CAP_SYS_ADMIN**：
//! - `systemd-run` 创建 transient unit 需特权；
//! - `systemctl stop` / `kill` / `reset-failed` 需特权（操作他人 unit）。
//!
//! 测试红线：**严禁碰宿主真实服务**——所有真实测用唯一 `osd-test-` 前缀 unit
//! （`TokioSystemdRunner::unit_name_for`），RAII guard 兜底清理 + `reset-failed`。
//!
//! ## 命令封装
//! | 编排器动作 | systemd 命令 |
//! |-----------|------------|
//! | `start_unit` | `systemd-run --unit=<name> --service-type=<t> [--remain] <ExecStart...>` |
//! | `stop_unit` | `systemctl stop <name>`（SIGTERM），超时后 `systemctl kill --signal=SIGKILL` |
//! | `is_active` | `systemctl is-active <name>`（返回 active/inactive/failed/unknown...） |
//! | `reset_failed` | `systemctl reset-failed <name>`（幂等清理失败态 unit） |
//!
//! `TokioSystemdRunner` 的方法用 `tokio::task::block_in_place` + `Handle::block_on`
//! 包裹 `tokio::process::Command`（与 [`crate::ntp_impl::ChronyRunner`] 同款），
//! 要求调用方运行在 multi-thread tokio runtime（生产 osd 与真实测均用 multi-thread）。
//! **故意不用 `output().await`**：参照 batch5 runc 子代理教训（init 后台进程继承管道
//! 致 `output().await` 永久 hang），systemctl 虽是前台命令，仍用 `spawn().wait()`
//! 逐段取 stdout/stderr，避免管道 EOF 边界条件。

use std::sync::Mutex;

use crate::ComponentId;
use crate::OrchestratorError;

/// systemd unit 类型（映射 systemd `Type=`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitType {
    /// `Type=oneshot` + `RemainAfterExit=yes`：跑完退出但 unit 留 active
    Oneshot,
    /// `Type=exec` / `Type=simple`：长期运行守护进程
    Exec,
}

impl UnitType {
    /// systemd-run 的 `--service-type=` 参数值
    fn service_type_arg(self) -> &'static str {
        match self {
            UnitType::Oneshot => "oneshot",
            UnitType::Exec => "exec",
        }
    }

    /// 是否需要 RemainAfterExit（oneshot 跑完留 active）
    fn want_remain(self) -> bool {
        matches!(self, UnitType::Oneshot)
    }
}

/// systemd 进程监管后端抽象
///
/// 抽象 `systemd-run` 创建 transient unit、`systemctl is-active`/`stop`/`reset-failed`
/// 的执行，便于单元测试用内存后端替身，避免真碰宿主 systemd（规格书 §9 红线）。
///
/// 实现者必须线程安全（`Send + Sync`）；`SystemdOrchestrator` 跨多组件并发调用。
/// 本 trait 用**同步签名**（非 async fn in trait），以支持 `dyn SystemdRunner` 派发
/// （与 [`crate::CgroupBackend`] / [`crate::ntp_impl::NtpRunner`] 风格一致）。
/// `SystemdOrchestrator` 的 async 方法内部用 `tokio::task::block_in_place` 包裹同步调用。
///
/// ## 单元名约定
/// 实现负责把 `<ComponentId>` 映射成宿主 systemd 的 unit 全名（如 `osd-<id>.service`）。
/// 测试用唯一前缀（`osd-test-<pid>-<nanos>-<id>`）避免并发冲突 + 兜底清理。
pub trait SystemdRunner: Send + Sync {
    /// 为组件计算宿主 systemd unit 全名
    ///
    /// 返回值供 `is_active` / `stop_unit` / `reset_failed` 使用，也供调试日志。
    /// 默认实现：`osd-<id>`（`.service` 后缀可选，systemctl 默认补全）。
    fn unit_name_for(&self, id: &ComponentId) -> String {
        format!("osd-{}", id.as_str())
    }

    /// 后端名（Debug / 日志用，如 `"InMemory(no-op)"` / `"Tokio(real)"`）
    fn backend_name(&self) -> &str;

    /// 创建 transient unit 并启动（`systemd-run --unit=...`）
    ///
    /// `exec_start` 为组件 ExecStart 拆分后的 argv（`[program, arg1, arg2, ...]`），
    /// 由调用方从 [`crate::ComponentDescriptor::command`] 解析得来。
    /// `unit_type` 决定 `--service-type=` 与是否 `--remain`。
    /// 失败（systemd-run 非 0 退出 / spawn 失败）返回 [`OrchestratorError::StartFailed`]。
    fn start_unit(
        &self,
        unit_name: &str,
        exec_start: &[String],
        unit_type: UnitType,
    ) -> Result<(), OrchestratorError>;

    /// 停止 unit（SIGTERM 优雅，超时 SIGKILL）并 reset-failed 清理
    ///
    /// 语义：`systemctl stop`（等进程退出），若 rc 非 0 再 `systemctl kill --signal=SIGKILL`，
    /// 最后 `systemctl reset-failed`（幂等）。失败返回 [`OrchestratorError::StopFailed`]。
    /// 幂等：unit 已 inactive / 未加载时返回 Ok（视为已停）。
    fn stop_unit(&self, unit_name: &str) -> Result<(), OrchestratorError>;

    /// 查询 unit 是否 active（`systemctl is-active`）
    ///
    /// 返回 stdout 去尾换行的状态字符串（`active`/`inactive`/`failed`/`activating`/`unknown`...）。
    /// 命令执行本身失败（spawn 失败）才返回 Err；`is-active` rc 非 0（inactive rc=3 等）
    /// 不算错误——状态由返回字符串表达。
    fn is_active(&self, unit_name: &str) -> Result<String, OrchestratorError>;

    /// 幂等清理失败态 unit（`systemctl reset-failed`）
    ///
    /// unit 已卸载时报 "not loaded"，属正常，返回 Ok。
    fn reset_failed(&self, unit_name: &str) -> Result<(), OrchestratorError>;
}

// ----------------------------------------------------------------------------
// 真实后端：tokio::process::Command 跑 systemctl / systemd-run（生产用，需 root）
// ----------------------------------------------------------------------------

/// systemd-run 可执行名（依赖 PATH；本机 systemd 259 在 `/usr/bin/systemd-run`）
const SYSTEMD_RUN_BIN: &str = "systemd-run";
/// systemctl 可执行名（依赖 PATH；本机 `/usr/bin/systemctl`）
const SYSTEMCTL_BIN: &str = "systemctl";
/// stop 后等 SIGTERM 退出的最大时长（毫秒）——超时则 SIGKILL
const STOP_GRACE_MS: u64 = 10_000;
/// start 后轮询 is-active 的超时时长（毫秒）
const START_POLL_TIMEOUT_MS: u64 = 15_000;
/// start 轮询间隔（毫秒）
const START_POLL_INTERVAL_MS: u64 = 100;

/// 基于 `tokio::process::Command` 的真实 systemd 编排后端
///
/// **权限**：所有写操作（`start_unit`/`stop_unit`/`reset_failed`）需 root + systemd
/// （规格书 §6 / §8）；`is_active` 仅读，非 root 也可但可能因权限看不到他人 unit。
///
/// 本结构无自身状态，所有调用都直接落系统命令；`Send + Sync` 由无状态保证。
#[derive(Debug, Default, Clone)]
pub struct TokioSystemdRunner {
    /// 单元名前缀（默认 `osd-`）；测试可注入唯一前缀（如 `osd-test-<pid>-<nanos>-`）
    /// 避免并发测同名冲突 + 兜底识别清理。
    unit_prefix: String,
}

impl TokioSystemdRunner {
    /// 构造（生产用，前缀默认 `osd-`）
    pub fn new() -> Self {
        Self::with_prefix("osd-")
    }

    /// 用自定义 unit 前缀构造（测试用，注入唯一 `osd-test-...` 前缀）
    pub fn with_prefix(unit_prefix: impl Into<String>) -> Self {
        Self {
            unit_prefix: unit_prefix.into(),
        }
    }
}

impl TokioSystemdRunner {
    /// 跑一个命令并返回 (exit_code, stdout_trimmed, stderr_trimmed)。
    ///
    /// spawn 失败 → Err(Io)；命令跑完（无论 exit code）→ Ok((code, out, err))。
    /// 调用方按命令语义解读 exit code（如 is-active rc=3 表示 inactive，不算错）。
    fn run_cmd_full(
        &self,
        bin: &str,
        args: &[&str],
    ) -> Result<(Option<i32>, String, String), OrchestratorError> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                let child = tokio::process::Command::new(bin)
                    .args(args)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .map_err(|e| {
                        OrchestratorError::Io(std::io::Error::other(format!(
                            "spawn {bin} 失败: {e}"
                        )))
                    })?;
                // systemctl/systemd-run 是前台命令，wait_with_output 安全（无 batch5 runc 管道坑）
                let out = child.wait_with_output().await.map_err(|e| {
                    OrchestratorError::Io(std::io::Error::other(format!("{bin} wait 失败: {e}")))
                })?;
                Ok((
                    out.status.code(),
                    String::from_utf8_lossy(&out.stdout).trim().to_string(),
                    String::from_utf8_lossy(&out.stderr).trim().to_string(),
                ))
            })
        })
    }

    /// 同步轮询 is-active 直到 active 或超时（block_in_place 内的忙等）。
    ///
    /// 返回 true=已 active；false=超时仍未 active（调用方决定是否判失败）。
    fn wait_until_active(&self, unit_name: &str) -> Result<bool, OrchestratorError> {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(START_POLL_TIMEOUT_MS);
        loop {
            let state = self.is_active(unit_name)?;
            if state == "active" {
                return Ok(true);
            }
            if std::time::Instant::now() >= deadline {
                return Ok(false);
            }
            // 短睡（block_in_place 内 std::thread::sleep 让出线程，不阻塞 runtime）
            std::thread::sleep(std::time::Duration::from_millis(START_POLL_INTERVAL_MS));
        }
    }
}

impl SystemdRunner for TokioSystemdRunner {
    fn unit_name_for(&self, id: &ComponentId) -> String {
        format!("{}{}", self.unit_prefix, id.as_str())
    }

    fn backend_name(&self) -> &str {
        "Tokio(real)"
    }

    fn start_unit(
        &self,
        unit_name: &str,
        exec_start: &[String],
        unit_type: UnitType,
    ) -> Result<(), OrchestratorError> {
        if exec_start.is_empty() {
            return Err(OrchestratorError::StartFailed {
                component: ComponentId::new(unit_name.to_string()),
                reason: "ExecStart 为空（组件无 command，且未提供占位命令）".into(),
            });
        }
        // 拼 systemd-run 参数：--unit=<name> --service-type=<t> [--remain] <argv...>
        let mut args: Vec<String> = vec![
            "--unit".into(),
            unit_name.into(),
            "--service-type".into(),
            unit_type.service_type_arg().into(),
        ];
        if unit_type.want_remain() {
            args.push("--remain".into());
        }
        args.extend(exec_start.iter().cloned());
        let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let (code, _out, stderr) = self.run_cmd_full(SYSTEMD_RUN_BIN, &arg_refs)?;
        if code == Some(0) {
            // systemd-run 成功提交后，轮询 is-active 确认真实拉起
            // （exec 类型需等 execve 成功才 active；oneshot+remain 跑完即 active）
            let active = self.wait_until_active(unit_name)?;
            if active {
                return Ok(());
            }
            return Err(OrchestratorError::StartFailed {
                component: ComponentId::new(unit_name.to_string()),
                reason: format!(
                    "systemd-run 成功但 is-active 轮询超时（{START_POLL_TIMEOUT_MS}ms 内未转 active）。stderr: {stderr}"
                ),
            });
        }
        Err(OrchestratorError::StartFailed {
            component: ComponentId::new(unit_name.to_string()),
            reason: format!(
                "systemd-run 退出码 {code:?}。stderr: {stderr}（确认 root + systemd 在跑）"
            ),
        })
    }

    fn stop_unit(&self, unit_name: &str) -> Result<(), OrchestratorError> {
        // 1. systemctl stop（SIGTERM 优雅，systemd 内部等 TimeoutStopSec 后才 SIGKILL）
        let (code, _out, stderr) = self.run_cmd_full(SYSTEMCTL_BIN, &["stop", unit_name])?;
        // stop rc=0 视为已停；非 0 可能是 unit 未加载（已停）或真失败
        if code != Some(0) {
            // 区分"未加载（已停，幂等 Ok）"与"真失败"
            let state = self.is_active(unit_name)?;
            if state == "inactive" || state == "unknown" || state == "dead" {
                // 已停 / 未加载：幂等 Ok
            } else {
                // 真没停下来：尝试 SIGKILL
                let (_kcode, _kout, _kerr) =
                    self.run_cmd_full(SYSTEMCTL_BIN, &["kill", unit_name, "--signal=SIGKILL"])?;
                // 再等一下确认
                std::thread::sleep(std::time::Duration::from_millis(STOP_GRACE_MS.min(500)));
                let state2 = self.is_active(unit_name)?;
                if state2 != "inactive" && state2 != "unknown" && state2 != "failed" {
                    return Err(OrchestratorError::StopFailed {
                        component: ComponentId::new(unit_name.to_string()),
                        reason: format!(
                            "systemctl stop 退出码 {code:?} 且 SIGKILL 后仍 {state2}。stderr: {stderr}"
                        ),
                    });
                }
            }
        }
        // 清理失败态（幂等；unit 已卸载时报 not loaded，属正常）
        self.reset_failed(unit_name)?;
        Ok(())
    }

    fn is_active(&self, unit_name: &str) -> Result<String, OrchestratorError> {
        let (_code, stdout, _stderr) =
            self.run_cmd_full(SYSTEMCTL_BIN, &["is-active", unit_name])?;
        // is-active rc 非 0（inactive rc=3 / failed rc=?）不算错误——状态由 stdout 表达
        Ok(stdout)
    }

    fn reset_failed(&self, unit_name: &str) -> Result<(), OrchestratorError> {
        let _ = self.run_cmd_full(SYSTEMCTL_BIN, &["reset-failed", unit_name])?;
        // reset-failed 幂等：unit 已卸载时报 not loaded，rc 非 0，属正常，返回 Ok
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// 内存后端：单元测试 / framework 锚点测替身（no-op，不碰 systemd）
// ----------------------------------------------------------------------------

/// 一条记录的 systemd 调用（供测试断言编排器是否真的调了 systemctl）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedCall {
    /// `start_unit(unit_name, exec_start_argv, unit_type)`
    StartUnit {
        unit_name: String,
        exec_start: Vec<String>,
        unit_type: UnitType,
    },
    /// `stop_unit(unit_name)`
    StopUnit { unit_name: String },
    /// `is_active(unit_name)`
    IsActive { unit_name: String },
    /// `reset_failed(unit_name)`
    ResetFailed { unit_name: String },
}

/// 内存后端：不执行任何真实 systemctl，仅记录调用 + 维护一份"unit → is-active 状态"映射。
///
/// **默认行为（向后兼容现有单元测）**：
/// - `start_unit`：记录调用，把 unit 状态置为 `active`（让框架状态机认为启动成功 → Running）。
/// - `stop_unit`：记录调用，把 unit 状态置为 `inactive`。
/// - `is_active`：返回映射中的状态；未 start 过的返回 `inactive`（与真实 systemctl 一致）。
/// - `reset_failed`：记录调用，no-op。
///
/// 这样现有 `impl_orchestrator.rs` 内联单测、`os-integration` 启动测、batch4 框架锚点测
/// 全部保持原语义（`do_start_inner` 后状态机为 Running），无需 root、不碰宿主 systemd。
///
/// 测试可通过 `set_active` / `set_failed` 注入故障场景（如让 start 后 is-active=failed）。
#[derive(Debug, Default)]
pub struct InMemorySystemdRunner {
    /// unit → is-active 状态（未列入则视为 `inactive`）
    states: Mutex<std::collections::HashMap<String, String>>,
    /// 调用记录（按发生顺序）
    calls: Mutex<Vec<RecordedCall>>,
    /// unit 名前缀（默认 `osd-`）
    unit_prefix: String,
}

impl InMemorySystemdRunner {
    /// 构造（前缀默认 `osd-`）
    pub fn new() -> Self {
        Self::with_prefix("osd-")
    }

    /// 用自定义前缀构造
    pub fn with_prefix(unit_prefix: impl Into<String>) -> Self {
        Self {
            states: Mutex::new(std::collections::HashMap::new()),
            calls: Mutex::new(Vec::new()),
            unit_prefix: unit_prefix.into(),
        }
    }

    /// 注入 unit 的 is-active 状态（测试用：模拟 failed / activating 等异常）
    pub fn set_active(&self, unit_name: &str, state: impl Into<String>) {
        self.states
            .lock()
            .expect("states poisoned")
            .insert(unit_name.to_string(), state.into());
    }

    /// 把 unit 标记为 failed（测试用：模拟 start 后立即崩溃）
    pub fn set_failed(&self, unit_name: &str) {
        self.set_active(unit_name, "failed");
    }

    /// 取所有调用记录的快照（测试断言用）
    pub fn recorded_calls(&self) -> Vec<RecordedCall> {
        self.calls.lock().expect("calls poisoned").clone()
    }

    /// 取指定 unit 的 is-active 快照状态（测试断言用；未列入返回 None）
    pub fn active_state(&self, unit_name: &str) -> Option<String> {
        self.states
            .lock()
            .expect("states poisoned")
            .get(unit_name)
            .cloned()
    }
}

impl SystemdRunner for InMemorySystemdRunner {
    fn unit_name_for(&self, id: &ComponentId) -> String {
        format!("{}{}", self.unit_prefix, id.as_str())
    }

    fn backend_name(&self) -> &str {
        "InMemory(no-op)"
    }

    fn start_unit(
        &self,
        unit_name: &str,
        exec_start: &[String],
        unit_type: UnitType,
    ) -> Result<(), OrchestratorError> {
        self.calls
            .lock()
            .expect("calls poisoned")
            .push(RecordedCall::StartUnit {
                unit_name: unit_name.to_string(),
                exec_start: exec_start.to_vec(),
                unit_type,
            });
        // 默认成功语义：start 后 unit 变 active（框架状态机 → Running）
        self.set_active(unit_name, "active");
        Ok(())
    }

    fn stop_unit(&self, unit_name: &str) -> Result<(), OrchestratorError> {
        self.calls
            .lock()
            .expect("calls poisoned")
            .push(RecordedCall::StopUnit {
                unit_name: unit_name.to_string(),
            });
        self.set_active(unit_name, "inactive");
        Ok(())
    }

    fn is_active(&self, unit_name: &str) -> Result<String, OrchestratorError> {
        self.calls
            .lock()
            .expect("calls poisoned")
            .push(RecordedCall::IsActive {
                unit_name: unit_name.to_string(),
            });
        let state = self
            .states
            .lock()
            .expect("states poisoned")
            .get(unit_name)
            .cloned()
            .unwrap_or_else(|| "inactive".into());
        Ok(state)
    }

    fn reset_failed(&self, unit_name: &str) -> Result<(), OrchestratorError> {
        self.calls
            .lock()
            .expect("calls poisoned")
            .push(RecordedCall::ResetFailed {
                unit_name: unit_name.to_string(),
            });
        // reset-failed 后 failed → inactive（与真实 systemctl 一致）
        let mut states = self.states.lock().expect("states poisoned");
        if states.get(unit_name).map(|s| s.as_str()) == Some("failed") {
            states.insert(unit_name.to_string(), "inactive".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_start_stop_roundtrip() {
        let r = InMemorySystemdRunner::new();
        let unit = "osd-test-unit";
        // 初始 inactive
        assert_eq!(r.is_active(unit).unwrap(), "inactive");
        // start → active
        r.start_unit(unit, &["/bin/sleep".into(), "60".into()], UnitType::Exec)
            .unwrap();
        assert_eq!(r.is_active(unit).unwrap(), "active");
        // stop → inactive
        r.stop_unit(unit).unwrap();
        assert_eq!(r.is_active(unit).unwrap(), "inactive");
        // 调用记录
        let calls = r.recorded_calls();
        assert!(calls
            .iter()
            .any(|c| matches!(c, RecordedCall::StartUnit { .. })));
        assert!(calls
            .iter()
            .any(|c| matches!(c, RecordedCall::StopUnit { .. })));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_set_failed_then_reset() {
        let r = InMemorySystemdRunner::new();
        let unit = "osd-x";
        r.set_failed(unit);
        assert_eq!(r.is_active(unit).unwrap(), "failed");
        r.reset_failed(unit).unwrap();
        assert_eq!(r.is_active(unit).unwrap(), "inactive");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unit_name_for_uses_prefix() {
        let r = InMemorySystemdRunner::with_prefix("osd-test-42-");
        assert_eq!(r.unit_name_for(&ComponentId::new("svc")), "osd-test-42-svc");
    }

    #[test]
    fn unit_type_service_type_arg() {
        assert_eq!(UnitType::Oneshot.service_type_arg(), "oneshot");
        assert_eq!(UnitType::Exec.service_type_arg(), "exec");
        assert!(UnitType::Oneshot.want_remain());
        assert!(!UnitType::Exec.want_remain());
    }

    // ---- 默认 unit_name_for（trait 默认实现） ----

    #[tokio::test(flavor = "multi_thread")]
    async fn default_unit_name_for_uses_osd_prefix() {
        // trait 默认实现：format!("osd-{}", id)
        let r = InMemorySystemdRunner::new();
        assert_eq!(r.unit_name_for(&ComponentId::new("storage")), "osd-storage");
    }

    // ---- InMemorySystemdRunner 各方法覆盖 ----

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_default_backend_name() {
        let r = InMemorySystemdRunner::new();
        assert_eq!(r.backend_name(), "InMemory(no-op)");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_default_prefix_is_osd() {
        // new() → 前缀 "osd-"
        let r = InMemorySystemdRunner::new();
        assert_eq!(r.unit_name_for(&ComponentId::new("x")), "osd-x");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_start_with_oneshot_records_type() {
        let r = InMemorySystemdRunner::new();
        let unit = "osd-oneshot-1";
        r.start_unit(unit, &["/bin/true".into()], UnitType::Oneshot)
            .unwrap();
        // 状态置 active
        assert_eq!(r.is_active(unit).unwrap(), "active");
        // 调用记录中包含 Oneshot 类型
        let calls = r.recorded_calls();
        assert!(calls.iter().any(|c| matches!(
            c,
            RecordedCall::StartUnit {
                unit_type: UnitType::Oneshot,
                ..
            }
        )));
        // argv 记录完整
        assert!(calls.iter().any(|c| matches!(
            c,
            RecordedCall::StartUnit { exec_start, .. } if exec_start == &["/bin/true".to_string()]
        )));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_start_with_exec_records_type() {
        let r = InMemorySystemdRunner::new();
        r.start_unit(
            "osd-exec-1",
            &["/bin/sleep".into(), "infinity".into()],
            UnitType::Exec,
        )
        .unwrap();
        let calls = r.recorded_calls();
        assert!(calls.iter().any(|c| matches!(
            c,
            RecordedCall::StartUnit {
                unit_type: UnitType::Exec,
                ..
            }
        )));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_is_active_unknown_returns_inactive() {
        // 未 start 过的 unit → "inactive"（与真实 systemctl 一致）
        let r = InMemorySystemdRunner::new();
        assert_eq!(r.is_active("never-started").unwrap(), "inactive");
        // 同时记一条 IsActive 调用
        assert!(r.recorded_calls().iter().any(|c| matches!(
            c,
            RecordedCall::IsActive { unit_name } if unit_name == "never-started"
        )));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_set_active_injects_custom_state() {
        let r = InMemorySystemdRunner::new();
        r.set_active("u", "activating");
        assert_eq!(r.is_active("u").unwrap(), "activating");
        // active_state 快照应反映注入
        assert_eq!(r.active_state("u"), Some("activating".to_string()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_active_state_missing_returns_none() {
        let r = InMemorySystemdRunner::new();
        assert!(r.active_state("missing").is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_reset_failed_no_op_on_non_failed() {
        // reset_failed 对非 failed 状态的 unit：no-op，不报错
        let r = InMemorySystemdRunner::new();
        r.set_active("u", "active");
        r.reset_failed("u").unwrap();
        // 状态保持 active
        assert_eq!(r.is_active("u").unwrap(), "active");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_reset_failed_records_call() {
        let r = InMemorySystemdRunner::new();
        r.reset_failed("cleanup-unit").unwrap();
        assert!(r.recorded_calls().iter().any(|c| matches!(
            c,
            RecordedCall::ResetFailed { unit_name } if unit_name == "cleanup-unit"
        )));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_recorded_calls_preserves_order() {
        // 调用顺序应被保留（start → is_active → stop → reset_failed）
        let r = InMemorySystemdRunner::new();
        let unit = "ordered";
        r.start_unit(unit, &["/bin/true".into()], UnitType::Oneshot)
            .unwrap();
        let _ = r.is_active(unit).unwrap();
        r.stop_unit(unit).unwrap();
        r.reset_failed(unit).unwrap();
        let calls = r.recorded_calls();
        // 至少 4 条，前 4 条按顺序匹配
        assert!(calls.len() >= 4);
        assert!(matches!(calls[0], RecordedCall::StartUnit { .. }));
        assert!(matches!(calls[1], RecordedCall::IsActive { .. }));
        assert!(matches!(calls[2], RecordedCall::StopUnit { .. }));
        assert!(matches!(calls[3], RecordedCall::ResetFailed { .. }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_start_overwrites_previous_active() {
        // 同一 unit 再次 start：覆盖之前状态
        let r = InMemorySystemdRunner::new();
        r.set_failed("u"); // 先 failed
        assert_eq!(r.is_active("u").unwrap(), "failed");
        r.start_unit("u", &["/bin/true".into()], UnitType::Oneshot)
            .unwrap();
        assert_eq!(r.is_active("u").unwrap(), "active");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn in_memory_stop_overwrites_active_to_inactive() {
        let r = InMemorySystemdRunner::new();
        r.set_active("u", "active");
        r.stop_unit("u").unwrap();
        assert_eq!(r.is_active("u").unwrap(), "inactive");
        // StopUnit 被记录
        assert!(r.recorded_calls().iter().any(|c| matches!(
            c,
            RecordedCall::StopUnit { unit_name } if unit_name == "u"
        )));
    }

    // ---- TokioSystemdRunner 构造 + 后端名（不真跑 systemctl） ----

    #[test]
    fn tokio_runner_new_uses_default_prefix() {
        let r = TokioSystemdRunner::new();
        // 默认前缀 "osd-"
        assert_eq!(r.unit_name_for(&ComponentId::new("api")), "osd-api");
    }

    #[test]
    fn tokio_runner_with_prefix_custom() {
        let r = TokioSystemdRunner::with_prefix("osd-test-99-");
        assert_eq!(r.unit_name_for(&ComponentId::new("svc")), "osd-test-99-svc");
    }

    #[test]
    fn tokio_runner_backend_name_is_real() {
        let r = TokioSystemdRunner::new();
        assert_eq!(r.backend_name(), "Tokio(real)");
    }

    #[test]
    fn tokio_runner_default_has_empty_prefix() {
        // #[derive(Default)]：String 默认 ""，故 unit_name_for 仅返回 id（无前缀）
        let r = TokioSystemdRunner::default();
        assert_eq!(r.unit_name_for(&ComponentId::new("x")), "x");
    }

    #[test]
    fn tokio_runner_clone_preserves_prefix() {
        let r = TokioSystemdRunner::with_prefix("clone-prefix-");
        let r2 = r.clone();
        assert_eq!(
            r.unit_name_for(&ComponentId::new("v")),
            r2.unit_name_for(&ComponentId::new("v"))
        );
    }
}
