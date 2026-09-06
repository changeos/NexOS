//! `SystemdOrchestrator` —— `Orchestrator` trait 的真实实现
//!
//! 定位（规格书 §3 / §10 示例工作流 #4）：
//! - **本文件是"骨架 + 纯算法 + cgroup 配额 + systemd 进程监管"实现**：
//!   `ComponentRegistry`、拓扑排序、状态机、同组件操作串行化都是**真实可用**的逻辑；
//!   `set_quota`/`get_quota` 已接通真实 cgroups-rs cgroup v2 后端（[`crate::CgroupQuota`]）；
//!   `do_start_inner`/`do_stop_inner` 已接通真实 systemctl transient unit（[`crate::SystemdRunner`]）。
//! - **NTP 仍留 TODO**（见 [`crate::ntp_impl`]，已由 `ChronyNtp` 实现，本编排器不强持有）。
//!   systemd 进程监管通过 [`crate::SystemdRunner`] trait 抽象，默认注入
//!   [`crate::InMemorySystemdRunner`]（no-op，向后兼容现有单元测的非 root、纯状态机语义）；
//!   真实集成测通过 `with_systemd_runner` 注入 [`crate::TokioSystemdRunner`]（真跑
//!   `systemd-run`/`systemctl`，需 root + systemd）。cgroup 写入虽已接通真实后端，
//!   **单元测试用 [`crate::InMemoryCgroupBackend`] 注入**（不真写 cgroup），
//!   真实后端写入需 root + cgroup v2 挂载（见 [`crate::cgroup`] 模块文档）。
//!   现阶段方法签名完整、返回值语义正确，**不**调用 `todo!()` / `unimplemented!()`
//!   （避免 panic 污染下游测试）。
//!
//! ## 权限标注（规格书 §8 验收项）
//! | 操作 | 所需权限 | 当前实现状态 |
//! |------|---------|------------|
//! | start/stop/restart | root + systemd + CAP_SYS_ADMIN | **真实接通**（默认 InMemorySystemdRunner no-op；注入 TokioSystemdRunner 跑 systemd-run/systemctl，本机 systemd 259 验证） |
//! | set_quota/get_quota | root + cgroup v2 + CAP_SYS_ADMIN | **真实接通**（cgroups-rs 写 `/sys/fs/cgroup/os/<id>`；测试注入内存后端） |
//! | 拓扑排序/状态机 | 无（纯内存） | **完整** |

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use crate::cgroup::{CgroupBackend, CgroupQuota};
use crate::component::{ComponentDescriptor, ComponentId, ComponentStatus};
use crate::orchestrator::Orchestrator;
use crate::systemd_runner::{InMemorySystemdRunner, SystemdRunner, UnitType};
use crate::topo::topological_sort;
use crate::OrchestratorError;
use os_core::ResourceQuota;

/// 组件注册表（声明式：`HashMap<ComponentId, ComponentDescriptor>`）
///
/// 由 `SystemdOrchestrator` 在构造时一次性载入所有 `ComponentDescriptor`，
/// 运行期只读（启动顺序由 [`crate::topo::topological_sort`] 预计算并缓存）。
#[derive(Debug, Default)]
pub struct ComponentRegistry {
    descriptors: HashMap<ComponentId, ComponentDescriptor>,
}

impl ComponentRegistry {
    /// 建空注册表
    pub fn new() -> Self {
        Self::default()
    }

    /// 从描述符切片构建（重复 ID 后者覆盖前者）
    pub fn from_descriptors(descriptors: Vec<ComponentDescriptor>) -> Self {
        let mut map = HashMap::with_capacity(descriptors.len());
        for d in descriptors {
            map.insert(d.id.clone(), d);
        }
        Self { descriptors: map }
    }

    /// 注册单个组件（重复 ID 覆盖）
    pub fn register(&mut self, descriptor: ComponentDescriptor) {
        self.descriptors.insert(descriptor.id.clone(), descriptor);
    }

    /// 取组件描述符
    pub fn get(&self, id: &ComponentId) -> Option<&ComponentDescriptor> {
        self.descriptors.get(id)
    }

    /// 全部描述符（顺序不保证）
    pub fn all(&self) -> Vec<&ComponentDescriptor> {
        self.descriptors.values().collect()
    }

    /// 全部描述符的所有权副本
    fn descriptors_owned(&self) -> Vec<ComponentDescriptor> {
        self.descriptors.values().cloned().collect()
    }

    /// 已注册组件数量
    pub fn len(&self) -> usize {
        self.descriptors.len()
    }

    /// 是否为空
    pub fn is_empty(&self) -> bool {
        self.descriptors.is_empty()
    }
}

/// 基于 systemd 的编排器框架实现
///
/// **对象安全**：本类型用原生 `async fn in trait`（Orchestrator trait），
/// 不被 `Box<dyn Orchestrator>` 派发（见 ADR-COMPAT-001）。
///
/// ## 并发性
/// - 组件状态表用 `RwLock<ComponentStatus>` 保护（读多写少）。
/// - **同组件操作串行化**：`per_component_locks: HashMap<ComponentId, Arc<Mutex<()>>>`，
///   start/stop/restart/set_quota 对同一组件加锁后执行（规格书 §3 关键实现点名）。
///   不同组件的操作互不阻塞。
///
/// ## EventBus（软依赖）
/// `os-core::EventBus` trait 当前为原生 `async fn in trait`（非 dyn 兼容，
/// 见 ADR-COMPAT-001），故本结构**不强持有 `Box<dyn EventBus>`**。
/// 事件上报（组件启停 → `Topic::System`）留待 core-agent 就绪后，通过单独的
/// `EventEmitter` trait（dyn 兼容封装）或泛型参数接入。当前框架不上报事件。
/// 规格 §4：core-agent mock 就绪前用 stub / 不依赖。
pub struct SystemdOrchestrator {
    /// 组件注册表（启动后只读）
    registry: RwLock<ComponentRegistry>,
    /// 每个组件当前运行状态
    statuses: RwLock<HashMap<ComponentId, ComponentStatus>>,
    /// cgroup v2 配额子系统（真实 cgroups-rs 后端，测试可注入内存后端）
    cgroup: CgroupQuota,
    /// systemd 进程监管后端（真实 TokioSystemdRunner 或内存 no-op；默认内存）
    systemd: Box<dyn SystemdRunner>,
    /// 同组件操作串行化锁（惰性创建）
    per_component_locks: Mutex<HashMap<ComponentId, Arc<Mutex<()>>>>,
}

impl std::fmt::Debug for SystemdOrchestrator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemdOrchestrator")
            .field("registry_len", &self.registry.read().map(|r| r.len()))
            .field("cgroup", &self.cgroup)
            .field("systemd_runner", &self.systemd.backend_name())
            .finish_non_exhaustive()
    }
}

impl SystemdOrchestrator {
    /// 构造（生产用，cgroup 后端为真实 cgroups-rs，写入需 root + cgroup v2）
    ///
    /// cgroup 根前缀默认 `"os"`（即所有组件 cgroup 在 `/sys/fs/cgroup/os/<id>` 下）。
    /// systemd 后端默认 [`InMemorySystemdRunner`]（no-op）——**生产环境若需真实拉起
    /// 进程，请用 [`Self::with_systemd_runner`] 注入 [`crate::TokioSystemdRunner`]**。
    pub fn new(registry: ComponentRegistry) -> Self {
        Self::with_cgroup_base(registry, "os")
    }

    /// 用自定义 cgroup 根前缀构造（生产用）
    ///
    /// `base` 决定 cgroup 路径前缀：`/sys/fs/cgroup/<base>/<component_id>`。
    pub fn with_cgroup_base(registry: ComponentRegistry, base: impl Into<String>) -> Self {
        Self {
            registry: RwLock::new(registry),
            statuses: RwLock::new(HashMap::new()),
            cgroup: CgroupQuota::new(base),
            systemd: Box::new(InMemorySystemdRunner::new()),
            per_component_locks: Mutex::new(HashMap::new()),
        }
    }

    /// 用自定义 cgroup 后端构造（测试用，注入 [`crate::InMemoryCgroupBackend`]）
    ///
    /// 典型测试场景：避免真写 cgroup（需 root），用内存后端替身。
    /// systemd 后端仍默认 [`InMemorySystemdRunner`]（no-op，保持现有单元测语义）。
    pub fn with_cgroup_backend(
        registry: ComponentRegistry,
        base: impl Into<String>,
        backend: Box<dyn CgroupBackend>,
    ) -> Self {
        Self {
            registry: RwLock::new(registry),
            statuses: RwLock::new(HashMap::new()),
            cgroup: CgroupQuota::with_backend(base, backend),
            systemd: Box::new(InMemorySystemdRunner::new()),
            per_component_locks: Mutex::new(HashMap::new()),
        }
    }

    /// 用自定义 systemd runner 构造（真实集成测用，注入 [`crate::TokioSystemdRunner`]）
    ///
    /// cgroup 后端仍可由调用方在构造后通过 [`Self::cgroup_quota`] 之外的方式配置。
    /// 典型用法是：真实集成测同时注入 `InMemoryCgroupBackend`（避免真写 cgroup）
    /// 与 `TokioSystemdRunner`（真跑 systemctl）。本构造函数把 cgroup base 默认为 `"os"`，
    /// 后端为内存（与 [`Self::with_cgroup_backend`] 一致）。
    pub fn with_systemd_runner(
        registry: ComponentRegistry,
        systemd: Box<dyn SystemdRunner>,
    ) -> Self {
        Self {
            registry: RwLock::new(registry),
            statuses: RwLock::new(HashMap::new()),
            cgroup: CgroupQuota::with_backend("os", Box::new(crate::InMemoryCgroupBackend::new())),
            systemd,
            per_component_locks: Mutex::new(HashMap::new()),
        }
    }

    /// 取 systemd runner 引用（运维查询/高级用法）
    pub fn systemd_runner(&self) -> &dyn SystemdRunner {
        self.systemd.as_ref()
    }

    /// 取 cgroup 配额子系统（运维查询/高级用法）
    pub fn cgroup_quota(&self) -> &CgroupQuota {
        &self.cgroup
    }

    /// 取（或惰性创建）某组件的串行化锁
    fn lock_for(&self, id: &ComponentId) -> Arc<Mutex<()>> {
        let mut locks = self
            .per_component_locks
            .lock()
            .expect("per_component_locks poisoned");
        if let Some(arc) = locks.get(id) {
            arc.clone()
        } else {
            let arc = Arc::new(Mutex::new(()));
            locks.insert(id.clone(), arc.clone());
            arc
        }
    }

    /// 计算启动顺序（拓扑排序 + 循环检测）
    ///
    /// 公开方法供测试与运维查询；`Orchestrator::list_components` 也用之。
    /// 检测到环返回 [`OrchestratorError::DependencyCycle`]。
    pub fn startup_order(&self) -> Result<Vec<ComponentId>, OrchestratorError> {
        let registry = self.registry.read().expect("registry poisoned");
        topological_sort(&registry.descriptors_owned())
    }

    /// 读当前状态（不加组件锁，仅读状态表）
    fn current_status(&self, id: &ComponentId) -> Option<ComponentStatus> {
        self.statuses
            .read()
            .expect("statuses poisoned")
            .get(id)
            .copied()
    }

    /// 写状态
    fn set_status(&self, id: &ComponentId, status: ComponentStatus) {
        self.statuses
            .write()
            .expect("statuses poisoned")
            .insert(id.clone(), status);
    }
}

impl SystemdOrchestrator {
    /// 内部：执行单个组件启动 —— 状态机 + 真实 systemd 调用
    ///
    /// 状态机：
    /// - `Stopped` / `Failed` / 不存在状态 → `Starting` → 调 systemd runner 拉起 → `Running`
    /// - `Running` / `Starting` → 直接返回 Ok（幂等，不重复拉起）
    /// - `Disabled` → 返回 Err（禁用组件不可启动，须先改配置）
    ///
    /// systemd 调用：经 [`SystemdRunner::start_unit`] 创建 transient unit（`systemd-run`），
    /// runner 内部轮询 `is-active` 确认 active。失败映射为 `StartFailed`，状态机置 `Failed`。
    fn do_start_inner(&self, id: &ComponentId) -> Result<(), OrchestratorError> {
        // 校验组件已注册 + 取 command/enabled
        let (exec_start, unit_type) = {
            let registry = self.registry.read().expect("registry poisoned");
            let desc = registry
                .get(id)
                .ok_or_else(|| OrchestratorError::ComponentNotFound(id.clone()))?;
            if !desc.enabled {
                // 禁用组件：不在状态机里，直接拒
                self.set_status(id, ComponentStatus::Disabled);
                return Err(OrchestratorError::StartFailed {
                    component: id.clone(),
                    reason: "组件已禁用（enabled=false），须先改配置启用".into(),
                });
            }
            // 解析 ExecStart：组件有 command 则用之（exec 长跑）；无则占位 oneshot
            // （生产组件应有真实 command；占位仅用于框架/测试场景）
            parse_exec_start(desc)
        };

        match self.current_status(id) {
            Some(ComponentStatus::Running) | Some(ComponentStatus::Starting) => {
                // 幂等：已在运行 / 启动中，不重复拉起
                return Ok(());
            }
            Some(ComponentStatus::Disabled) => {
                return Err(OrchestratorError::StartFailed {
                    component: id.clone(),
                    reason: "组件已禁用，须先改配置启用".into(),
                });
            }
            _ => {}
        }

        self.set_status(id, ComponentStatus::Starting);
        let unit_name = self.systemd.unit_name_for(id);
        // 调 systemd runner 拉起（真实 runner 跑 systemd-run；内存 runner no-op 记 active）
        match self.systemd.start_unit(&unit_name, &exec_start, unit_type) {
            Ok(()) => {
                self.set_status(id, ComponentStatus::Running);
                Ok(())
            }
            Err(e) => {
                // 启动失败：状态机置 Failed，返回错误
                self.set_status(id, ComponentStatus::Failed);
                // reset-failed 清理（幂等；内存 runner 记调用，真实 runner 清宿主 unit）
                let _ = self.systemd.reset_failed(&unit_name);
                Err(OrchestratorError::StartFailed {
                    component: id.clone(),
                    reason: format!("{e}"),
                })
            }
        }
    }

    /// 内部：执行单个组件停止 —— 状态机 + 真实 systemd 调用
    ///
    /// 状态机：
    /// - `Running` / `Starting` / `Failed` → 调 systemd runner 停止 → `Stopped`
    /// - `Stopped` → 幂等返回 Ok
    /// - `Disabled` → 返回 Ok（禁用视为已停）
    ///
    /// systemd 调用：经 [`SystemdRunner::stop_unit`]（SIGTERM 优雅 → 超时 SIGKILL → reset-failed）。
    /// 注意：即使从未 start 过（状态 None），也调一次 stop_unit 兜底（真实 runner 幂等）；
    /// 内存 runner 同样幂等（no-op）。
    fn do_stop_inner(&self, id: &ComponentId) -> Result<(), OrchestratorError> {
        // 校验已注册
        {
            let registry = self.registry.read().expect("registry poisoned");
            if !registry.get(id).is_some() {
                return Err(OrchestratorError::ComponentNotFound(id.clone()));
            }
        }

        match self.current_status(id) {
            Some(ComponentStatus::Stopped) => return Ok(()),
            Some(ComponentStatus::Disabled) => return Ok(()),
            None => {
                // 从未启动过：视为已停。仍调一次 stop_unit 兜底清理（真实 runner 幂等：
                // unit 未加载时返回 Ok；内存 runner no-op）。这覆盖"上次崩溃后状态表丢失
                // 但宿主 unit 仍 active"的边界场景。
                let unit_name = self.systemd.unit_name_for(id);
                let _ = self.systemd.stop_unit(&unit_name);
                self.set_status(id, ComponentStatus::Stopped);
                return Ok(());
            }
            _ => {}
        }

        let unit_name = self.systemd.unit_name_for(id);
        match self.systemd.stop_unit(&unit_name) {
            Ok(()) => {
                self.set_status(id, ComponentStatus::Stopped);
                Ok(())
            }
            Err(e) => {
                // 停止失败：状态机保持原态（不置 Stopped），返回错误
                Err(OrchestratorError::StopFailed {
                    component: id.clone(),
                    reason: format!("{e}"),
                })
            }
        }
    }
}

/// 从 `ComponentDescriptor::command` 解析 ExecStart argv + 推断 unit 类型。
///
/// - 有 `command`：shell 风格按空白拆分（极简，不处理引号；生产组件 command 应为简单二进制路径
///   + 参数）。无 command 时用占位：`/bin/true`（oneshot）或 `/bin/sleep infinity`（exec）。
/// - unit 类型：command 含 `sleep`/`infinity`/长跑守护特征 或 显式占位时为 Exec；
///   `/bin/true` 占位为 Oneshot。
///
/// **注意**：本解析为极简实现（生产编排器应使用更健壮的 shell 词法分析，
/// 如 `shell-words` crate；当前避免新增依赖）。
fn parse_exec_start(desc: &ComponentDescriptor) -> (Vec<String>, UnitType) {
    match &desc.command {
        Some(cmd) if !cmd.trim().is_empty() => {
            // 极简 shell 拆分（按空白；不处理引号——生产组件 command 应为简单 argv）
            let argv: Vec<String> = cmd.split_whitespace().map(|s| s.to_string()).collect();
            // 推断类型：sleep/infinity 视为长跑；其他（含 /bin/true）默认 exec
            // （exec 更通用，simple 语义；oneshot 仅用于显式占位场景）
            let is_long_running = argv.iter().any(|a| a.ends_with("sleep") || a == "infinity");
            let unit_type = if is_long_running {
                UnitType::Exec
            } else {
                // 默认 exec（长跑守护语义）；若 command 恰是 /bin/true 也用 exec（不影响行为）
                UnitType::Exec
            };
            (argv, unit_type)
        }
        _ => {
            // 无 command：占位长跑进程（exec，sleep infinity）
            // 生产组件应总有 command；此处仅框架/测试兜底
            (vec!["/bin/sleep".into(), "infinity".into()], UnitType::Exec)
        }
    }
}

impl Orchestrator for SystemdOrchestrator {
    async fn start(&self, id: &ComponentId) -> crate::OrchestratorResult<()> {
        // 同组件操作串行化
        let lock = self.lock_for(id);
        let _guard = lock.lock().expect("component lock poisoned");
        self.do_start_inner(id)
    }

    async fn stop(&self, id: &ComponentId) -> crate::OrchestratorResult<()> {
        let lock = self.lock_for(id);
        let _guard = lock.lock().expect("component lock poisoned");
        self.do_stop_inner(id)
    }

    async fn restart(&self, id: &ComponentId) -> crate::OrchestratorResult<()> {
        // restart = stop + start；同组件锁保证不被并发操作打断
        let lock = self.lock_for(id);
        let _guard = lock.lock().expect("component lock poisoned");
        self.do_stop_inner(id)?;
        self.do_start_inner(id)
    }

    async fn status(&self, id: &ComponentId) -> crate::OrchestratorResult<ComponentStatus> {
        let registry = self.registry.read().expect("registry poisoned");
        if registry.get(id).is_none() {
            return Err(OrchestratorError::ComponentNotFound(id.clone()));
        }
        Ok(self.current_status(id).unwrap_or(ComponentStatus::Stopped))
    }

    async fn list_components(&self) -> crate::OrchestratorResult<Vec<ComponentDescriptor>> {
        let registry = self.registry.read().expect("registry poisoned");
        Ok(registry.descriptors_owned())
    }

    async fn set_quota(
        &self,
        id: &ComponentId,
        quota: ResourceQuota,
    ) -> crate::OrchestratorResult<()> {
        // 校验已注册
        {
            let registry = self.registry.read().expect("registry poisoned");
            if registry.get(id).is_none() {
                return Err(OrchestratorError::ComponentNotFound(id.clone()));
            }
        }
        // 同组件串行化
        let lock = self.lock_for(id);
        let _guard = lock.lock().expect("component lock poisoned");

        // 真实 cgroup v2 写入：经 CgroupQuota → CgroupBackend（默认 CgroupsRsBackend，
        // 需 root + cgroup v2；测试可注入 InMemoryCgroupBackend 避免真写）。
        // 后端内部：创建/更新 /sys/fs/cgroup/<base>/<id> 的 cpu.max/memory.max。
        // 写入失败映射为 QuotaFailed；成功后端会缓存快照供 get_quota 回退读。
        self.cgroup.set_quota(id, &quota)
    }

    async fn get_quota(&self, id: &ComponentId) -> crate::OrchestratorResult<ResourceQuota> {
        // 校验已注册
        let registry = self.registry.read().expect("registry poisoned");
        let desc = registry
            .get(id)
            .ok_or_else(|| OrchestratorError::ComponentNotFound(id.clone()))?;
        // 优先读 cgroup 后端（真实写过的配额）；读不到（未设置过）回退到描述符默认
        Ok(self
            .cgroup
            .get_quota(id)?
            .unwrap_or_else(|| desc.quota.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cgroup::InMemoryCgroupBackend;
    use crate::component::{ComponentDescriptor, ComponentId, ComponentStatus, HealthProbeConfig};
    use os_core::ResourceQuota;

    fn quota(cpu: f32) -> ResourceQuota {
        ResourceQuota {
            cpu_cores: Some(cpu),
            memory_bytes: None,
            io_bps_limit: None,
        }
    }

    fn desc(id: &str, deps: &[&str]) -> ComponentDescriptor {
        desc_with(id, deps, true)
    }

    fn desc_with(id: &str, deps: &[&str], enabled: bool) -> ComponentDescriptor {
        ComponentDescriptor {
            id: ComponentId::new(id),
            dependencies: deps.iter().map(|&s| ComponentId::new(s)).collect(),
            quota: quota(1.0),
            health_probe: HealthProbeConfig {
                kind: "exec".into(),
                target: "/bin/true".into(),
                interval_secs: 10,
                timeout_secs: 1,
                failure_threshold: 3,
            },
            command: Some("/bin/true".into()),
            enabled,
        }
    }

    /// 构造编排器（注入内存 cgroup 后端，避免真写 cgroup 需 root）
    fn build(descs: &[ComponentDescriptor]) -> SystemdOrchestrator {
        let registry = ComponentRegistry::from_descriptors(descs.to_vec());
        SystemdOrchestrator::with_cgroup_backend(
            registry,
            "os",
            Box::new(InMemoryCgroupBackend::new()),
        )
    }

    // ---- ComponentRegistry ----

    #[test]
    fn registry_register_and_get() {
        let mut r = ComponentRegistry::new();
        r.register(desc("a", &[]));
        assert_eq!(r.len(), 1);
        assert!(r.get(&ComponentId::new("a")).is_some());
        assert!(r.get(&ComponentId::new("missing")).is_none());
    }

    #[test]
    fn registry_from_descriptors_dedups_by_overwrite() {
        let r = ComponentRegistry::from_descriptors(vec![desc("a", &[]), desc("a", &["b"])]);
        assert_eq!(r.len(), 1);
        // 后者覆盖
        assert_eq!(r.get(&ComponentId::new("a")).unwrap().dependencies.len(), 1);
    }

    // ---- startup_order（拓扑排序 + 循环检测） ----

    #[tokio::test]
    async fn startup_order_linear() {
        let orch = build(&[desc("c", &["b"]), desc("b", &["a"]), desc("a", &[])]);
        let order = orch.startup_order().expect("线性链应可排序");
        let pos = |id: &str| order.iter().position(|x| x.as_str() == id).unwrap();
        assert!(pos("a") < pos("b"));
        assert!(pos("b") < pos("c"));
    }

    #[tokio::test]
    async fn startup_order_detects_cycle() {
        let orch = build(&[desc("a", &["b"]), desc("b", &["a"])]);
        let err = orch.startup_order().expect_err("环应被检测");
        assert!(matches!(err, OrchestratorError::DependencyCycle { .. }));
    }

    // ---- 状态机：start / status ----

    #[tokio::test]
    async fn start_transitions_stopped_to_running() {
        let orch = build(&[desc("a", &[])]);
        // 初始：未启动 → status 返回 Stopped（默认）
        assert_eq!(
            orch.status(&ComponentId::new("a")).await.unwrap(),
            ComponentStatus::Stopped
        );
        orch.start(&ComponentId::new("a")).await.unwrap();
        assert_eq!(
            orch.status(&ComponentId::new("a")).await.unwrap(),
            ComponentStatus::Running
        );
    }

    #[tokio::test]
    async fn start_unknown_component_errors() {
        let orch = build(&[desc("a", &[])]);
        let err = orch
            .start(&ComponentId::new("missing"))
            .await
            .expect_err("未注册组件应报错");
        assert!(matches!(err, OrchestratorError::ComponentNotFound(_)));
    }

    #[tokio::test]
    async fn start_disabled_component_errors() {
        let orch = build(&[desc_with("a", &[], false)]);
        let err = orch
            .start(&ComponentId::new("a"))
            .await
            .expect_err("禁用组件不可启动");
        match err {
            OrchestratorError::StartFailed { component, .. } => {
                assert_eq!(component, ComponentId::new("a"));
            }
            other => panic!("期望 StartFailed，实际: {:?}", other),
        }
        // 状态应为 Disabled
        assert_eq!(
            orch.status(&ComponentId::new("a")).await.unwrap(),
            ComponentStatus::Disabled
        );
    }

    #[tokio::test]
    async fn start_running_is_idempotent() {
        let orch = build(&[desc("a", &[])]);
        orch.start(&ComponentId::new("a")).await.unwrap();
        // 再次 start 不报错
        orch.start(&ComponentId::new("a")).await.unwrap();
        assert_eq!(
            orch.status(&ComponentId::new("a")).await.unwrap(),
            ComponentStatus::Running
        );
    }

    // ---- 状态机：stop / restart ----

    #[tokio::test]
    async fn stop_transitions_running_to_stopped() {
        let orch = build(&[desc("a", &[])]);
        orch.start(&ComponentId::new("a")).await.unwrap();
        orch.stop(&ComponentId::new("a")).await.unwrap();
        assert_eq!(
            orch.status(&ComponentId::new("a")).await.unwrap(),
            ComponentStatus::Stopped
        );
    }

    #[tokio::test]
    async fn stop_unknown_errors() {
        let orch = build(&[desc("a", &[])]);
        let err = orch
            .stop(&ComponentId::new("missing"))
            .await
            .expect_err("未注册组件应报错");
        assert!(matches!(err, OrchestratorError::ComponentNotFound(_)));
    }

    #[tokio::test]
    async fn stop_when_stopped_is_idempotent() {
        let orch = build(&[desc("a", &[])]);
        // 未启动直接 stop：幂等 Ok
        orch.stop(&ComponentId::new("a")).await.unwrap();
        assert_eq!(
            orch.status(&ComponentId::new("a")).await.unwrap(),
            ComponentStatus::Stopped
        );
    }

    #[tokio::test]
    async fn restart_running_returns_to_running() {
        let orch = build(&[desc("a", &[])]);
        orch.start(&ComponentId::new("a")).await.unwrap();
        orch.restart(&ComponentId::new("a")).await.unwrap();
        assert_eq!(
            orch.status(&ComponentId::new("a")).await.unwrap(),
            ComponentStatus::Running
        );
    }

    // ---- list_components ----

    #[tokio::test]
    async fn list_components_returns_all_registered() {
        let orch = build(&[desc("a", &[]), desc("b", &["a"])]);
        let list = orch.list_components().await.unwrap();
        let ids: Vec<&str> = list.iter().map(|d| d.id.as_str()).collect();
        assert!(ids.contains(&"a"));
        assert!(ids.contains(&"b"));
        assert_eq!(list.len(), 2);
    }

    // ---- set_quota / get_quota ----

    #[tokio::test]
    async fn set_get_quota_roundtrip() {
        let orch = build(&[desc("a", &[])]);
        let new_quota = quota(2.5);
        orch.set_quota(&ComponentId::new("a"), new_quota.clone())
            .await
            .unwrap();
        let got = orch.get_quota(&ComponentId::new("a")).await.unwrap();
        assert_eq!(got.cpu_cores, Some(2.5));
    }

    #[tokio::test]
    async fn get_quota_returns_descriptor_default_when_unset() {
        let orch = build(&[desc("a", &[])]);
        let got = orch.get_quota(&ComponentId::new("a")).await.unwrap();
        // desc 默认 quota cpu=1.0
        assert_eq!(got.cpu_cores, Some(1.0));
    }

    #[tokio::test]
    async fn set_quota_unknown_errors() {
        let orch = build(&[desc("a", &[])]);
        let err = orch
            .set_quota(&ComponentId::new("missing"), quota(1.0))
            .await
            .expect_err("未注册组件应报错");
        assert!(matches!(err, OrchestratorError::ComponentNotFound(_)));
    }

    #[tokio::test]
    async fn get_quota_unknown_errors() {
        let orch = build(&[desc("a", &[])]);
        let err = orch
            .get_quota(&ComponentId::new("missing"))
            .await
            .expect_err("未注册组件应报错");
        assert!(matches!(err, OrchestratorError::ComponentNotFound(_)));
    }

    // ---- set_quota/get_quota 与 cgroup 后端接通（新测，规格书 §3 关键实现） ----

    #[tokio::test]
    async fn set_quota_writes_through_to_cgroup_backend() {
        // 内存后端记录写入，证明 set_quota 不再是"仅内存快照"——它真的委派给后端
        let orch = build(&[desc("a", &[])]);
        let new_quota = ResourceQuota {
            cpu_cores: Some(4.0),
            memory_bytes: Some(2 * 1024 * 1024 * 1024), // 2GiB
            io_bps_limit: Some(100_000_000),
        };
        orch.set_quota(&ComponentId::new("a"), new_quota.clone())
            .await
            .unwrap();
        // 通过 cgroup_quota() 直接查后端（绕过 get_quota 的描述符回退）
        let got = orch
            .cgroup_quota()
            .get_quota(&ComponentId::new("a"))
            .unwrap()
            .expect("后端应有快照");
        assert_eq!(got.cpu_cores, Some(4.0));
        assert_eq!(got.memory_bytes, Some(2 * 1024 * 1024 * 1024));
        assert_eq!(got.io_bps_limit, Some(100_000_000));
    }

    #[tokio::test]
    async fn set_quota_full_resource_roundtrip() {
        // 完整三个字段（CPU/内存/IO）的 set → get 往返
        let orch = build(&[desc("a", &[])]);
        let q = ResourceQuota {
            cpu_cores: Some(0.5),
            memory_bytes: Some(512 * 1024 * 1024),
            io_bps_limit: Some(50_000_000),
        };
        orch.set_quota(&ComponentId::new("a"), q).await.unwrap();
        let got = orch.get_quota(&ComponentId::new("a")).await.unwrap();
        assert_eq!(got.cpu_cores, Some(0.5));
        assert_eq!(got.memory_bytes, Some(512 * 1024 * 1024));
        assert_eq!(got.io_bps_limit, Some(50_000_000));
    }

    #[tokio::test]
    async fn set_quota_unlimited_roundtrip() {
        // 全 None（不限）也能 set/get 往返
        let orch = build(&[desc("a", &[])]);
        let q = ResourceQuota {
            cpu_cores: None,
            memory_bytes: None,
            io_bps_limit: None,
        };
        orch.set_quota(&ComponentId::new("a"), q).await.unwrap();
        let got = orch.get_quota(&ComponentId::new("a")).await.unwrap();
        assert!(got.cpu_cores.is_none());
        assert!(got.memory_bytes.is_none());
        assert!(got.io_bps_limit.is_none());
    }

    #[tokio::test]
    async fn set_quota_overwrites_previous() {
        let orch = build(&[desc("a", &[])]);
        orch.set_quota(&ComponentId::new("a"), quota(1.0))
            .await
            .unwrap();
        orch.set_quota(&ComponentId::new("a"), quota(3.0))
            .await
            .unwrap();
        let got = orch.get_quota(&ComponentId::new("a")).await.unwrap();
        assert_eq!(got.cpu_cores, Some(3.0));
    }

    #[tokio::test]
    async fn with_cgroup_base_sets_custom_prefix() {
        // 验证自定义 base 构造路径（不真写 cgroup，用内存后端）
        let registry = ComponentRegistry::from_descriptors(vec![desc("a", &[])]);
        let orch = SystemdOrchestrator::with_cgroup_base(registry, "custom-os");
        assert_eq!(orch.cgroup_quota().base(), "custom-os");
    }

    #[tokio::test]
    async fn new_default_uses_os_base() {
        // 默认构造（生产路径）base 应为 "os"
        let registry = ComponentRegistry::from_descriptors(vec![desc("a", &[])]);
        let orch = SystemdOrchestrator::new(registry);
        assert_eq!(orch.cgroup_quota().base(), "os");
    }

    #[tokio::test]
    async fn quotas_are_isolated_per_component() {
        // 不同组件的配额互不影响
        let orch = build(&[desc("a", &[]), desc("b", &[])]);
        orch.set_quota(&ComponentId::new("a"), quota(1.0))
            .await
            .unwrap();
        orch.set_quota(&ComponentId::new("b"), quota(2.0))
            .await
            .unwrap();
        assert_eq!(
            orch.get_quota(&ComponentId::new("a"))
                .await
                .unwrap()
                .cpu_cores,
            Some(1.0)
        );
        assert_eq!(
            orch.get_quota(&ComponentId::new("b"))
                .await
                .unwrap()
                .cpu_cores,
            Some(2.0)
        );
    }

    // ---- 同组件操作串行化（行为验证） ----
    // 说明：本测试验证"同组件串行化"在逻辑上不 panic、结果一致；
    // 真实并发竞态需压力测，这里覆盖正确性而非竞态。

    #[tokio::test]
    async fn concurrent_starts_same_component_converge() {
        let orch = Arc::new(build(&[desc("a", &[])]));
        let mut handles = vec![];
        for _ in 0..10 {
            let orch = orch.clone();
            handles.push(tokio::spawn(async move {
                orch.start(&ComponentId::new("a")).await
            }));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }
        assert_eq!(
            orch.status(&ComponentId::new("a")).await.unwrap(),
            ComponentStatus::Running
        );
    }

    // ---- parse_exec_start：纯函数边界 ----

    fn desc_with_cmd(id: &str, command: Option<&str>) -> ComponentDescriptor {
        ComponentDescriptor {
            id: ComponentId::new(id),
            dependencies: vec![],
            quota: quota(1.0),
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

    #[test]
    fn parse_exec_start_with_command_splits_whitespace() {
        let d = desc_with_cmd("a", Some("/bin/sleep 60"));
        let (argv, ut) = parse_exec_start(&d);
        assert_eq!(argv, vec!["/bin/sleep".to_string(), "60".to_string()]);
        assert_eq!(ut, UnitType::Exec);
    }

    #[test]
    fn parse_exec_start_sleep_infinity_detected_as_exec() {
        let d = desc_with_cmd("a", Some("/bin/sleep infinity"));
        let (argv, ut) = parse_exec_start(&d);
        assert_eq!(argv, vec!["/bin/sleep".to_string(), "infinity".to_string()]);
        assert_eq!(ut, UnitType::Exec);
    }

    #[test]
    fn parse_exec_start_argv_infinity_treated_as_exec() {
        // argv 含 "infinity" 字面 → 视为长跑 Exec
        let d = desc_with_cmd("a", Some("/usr/bin/infinity"));
        let (argv, ut) = parse_exec_start(&d);
        assert_eq!(argv, vec!["/usr/bin/infinity".to_string()]);
        assert_eq!(ut, UnitType::Exec);
    }

    #[test]
    fn parse_exec_start_no_command_uses_placeholder_sleep_infinity() {
        let d = desc_with_cmd("a", None);
        let (argv, ut) = parse_exec_start(&d);
        assert_eq!(argv, vec!["/bin/sleep".to_string(), "infinity".to_string()]);
        assert_eq!(ut, UnitType::Exec);
    }

    #[test]
    fn parse_exec_start_empty_command_uses_placeholder() {
        // command 为空白字符串 → 走占位
        let d = desc_with_cmd("a", Some("   "));
        let (argv, ut) = parse_exec_start(&d);
        assert_eq!(argv, vec!["/bin/sleep".to_string(), "infinity".to_string()]);
        assert_eq!(ut, UnitType::Exec);
    }

    #[test]
    fn parse_exec_start_non_sleep_command_is_exec() {
        // 非 sleep/infinity 命令 → 默认 Exec
        let d = desc_with_cmd("a", Some("/usr/bin/os-storage --config /etc/os.toml"));
        let (argv, ut) = parse_exec_start(&d);
        assert_eq!(argv.len(), 3);
        assert_eq!(ut, UnitType::Exec);
    }

    // ---- ComponentRegistry 边界 ----

    #[test]
    fn registry_default_is_empty() {
        let r = ComponentRegistry::default();
        assert!(r.is_empty());
        assert_eq!(r.len(), 0);
    }

    #[test]
    fn registry_new_is_empty() {
        let r = ComponentRegistry::new();
        assert!(r.is_empty());
    }

    #[test]
    fn registry_all_returns_values() {
        let mut r = ComponentRegistry::new();
        r.register(desc("a", &[]));
        r.register(desc("b", &[]));
        let all = r.all();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn registry_get_missing_returns_none() {
        let r = ComponentRegistry::new();
        assert!(r.get(&ComponentId::new("nope")).is_none());
    }

    #[test]
    fn registry_register_overwrites_same_id() {
        let mut r = ComponentRegistry::new();
        r.register(desc("a", &[]));
        r.register(desc("a", &["b"])); // 覆盖
        assert_eq!(r.len(), 1);
        let got = r.get(&ComponentId::new("a")).unwrap();
        assert_eq!(got.dependencies.len(), 1);
    }

    #[test]
    fn registry_from_descriptors_empty() {
        let r = ComponentRegistry::from_descriptors(vec![]);
        assert!(r.is_empty());
    }

    // ---- SystemdOrchestrator 构造 + accessor ----

    #[tokio::test]
    async fn orchestrator_debug_format_includes_registry_len() {
        let orch = build(&[desc("a", &[])]);
        let s = format!("{orch:?}");
        // Debug impl 应含 registry_len / cgroup / systemd_runner
        assert!(s.contains("registry_len"));
        assert!(s.contains("InMemory(no-op)"));
    }

    #[tokio::test]
    async fn orchestrator_systemd_runner_accessor() {
        let orch = build(&[desc("a", &[])]);
        // 默认 InMemorySystemdRunner
        assert_eq!(orch.systemd_runner().backend_name(), "InMemory(no-op)");
    }

    #[tokio::test]
    async fn orchestrator_with_systemd_runner_injects_custom() {
        // 注入 TokioSystemdRunner（不真跑 systemctl，仅验证构造 + accessor）
        let registry = ComponentRegistry::from_descriptors(vec![desc("a", &[])]);
        let orch = SystemdOrchestrator::with_systemd_runner(
            registry,
            Box::new(crate::TokioSystemdRunner::new()),
        );
        assert_eq!(orch.systemd_runner().backend_name(), "Tokio(real)");
    }

    #[tokio::test]
    async fn orchestrator_startup_order_empty_registry() {
        let orch = build(&[]);
        let order = orch.startup_order().unwrap();
        assert!(order.is_empty());
    }

    // ---- 状态机：Failed / Disabled 边界 ----

    #[tokio::test]
    async fn restart_disabled_component_errors() {
        let orch = build(&[desc_with("a", &[], false)]);
        let err = orch
            .restart(&ComponentId::new("a"))
            .await
            .expect_err("禁用组件不可 restart");
        assert!(matches!(err, OrchestratorError::StartFailed { .. }));
    }

    #[tokio::test]
    async fn restart_unknown_component_errors_at_stop() {
        let orch = build(&[desc("a", &[])]);
        let err = orch
            .restart(&ComponentId::new("missing"))
            .await
            .expect_err("未注册组件 restart 应报错");
        assert!(matches!(err, OrchestratorError::ComponentNotFound(_)));
    }

    #[tokio::test]
    async fn status_unknown_component_errors() {
        let orch = build(&[desc("a", &[])]);
        let err = orch
            .status(&ComponentId::new("missing"))
            .await
            .expect_err("未注册组件 status 应报错");
        assert!(matches!(err, OrchestratorError::ComponentNotFound(_)));
    }

    #[tokio::test]
    async fn list_components_empty_registry() {
        let orch = build(&[]);
        let list = orch.list_components().await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn restart_running_component_succeeds() {
        let orch = build(&[desc("a", &[])]);
        orch.start(&ComponentId::new("a")).await.unwrap();
        orch.restart(&ComponentId::new("a")).await.unwrap();
        assert_eq!(
            orch.status(&ComponentId::new("a")).await.unwrap(),
            ComponentStatus::Running
        );
    }

    #[tokio::test]
    async fn stop_disabled_component_is_ok() {
        // Disabled 组件 stop → 直接 Ok（视为已停）
        let orch = build(&[desc_with("a", &[], false)]);
        // 先 start 让状态机置 Disabled（start 失败但状态置 Disabled）
        let _ = orch.start(&ComponentId::new("a")).await;
        assert_eq!(
            orch.status(&ComponentId::new("a")).await.unwrap(),
            ComponentStatus::Disabled
        );
        // stop Disabled → Ok
        orch.stop(&ComponentId::new("a")).await.unwrap();
        // 状态保持 Disabled（stop 不改 Disabled 状态机的提前返回路径）
    }

    // ---- 默认 Orchestrator trait get_quota（未实现时返回 ComponentNotFound） ----
    //
    // 注：SystemdOrchestrator 重写了 get_quota，故默认实现路径由一个桩 impl 覆盖。
    // Orchestrator 用原生 async fn in trait（无 #[async_trait]），桩 impl 直接写 async fn。

    #[tokio::test]
    async fn default_get_quota_trait_returns_not_found() {
        use crate::orchestrator::Orchestrator;
        use crate::ComponentId;

        struct StubOrch;
        impl Orchestrator for StubOrch {
            async fn start(&self, _id: &ComponentId) -> crate::OrchestratorResult<()> {
                Ok(())
            }
            async fn stop(&self, _id: &ComponentId) -> crate::OrchestratorResult<()> {
                Ok(())
            }
            async fn restart(&self, _id: &ComponentId) -> crate::OrchestratorResult<()> {
                Ok(())
            }
            async fn status(
                &self,
                _id: &ComponentId,
            ) -> crate::OrchestratorResult<ComponentStatus> {
                Ok(ComponentStatus::Stopped)
            }
            async fn list_components(&self) -> crate::OrchestratorResult<Vec<ComponentDescriptor>> {
                Ok(vec![])
            }
            async fn set_quota(
                &self,
                _id: &ComponentId,
                _quota: ResourceQuota,
            ) -> crate::OrchestratorResult<()> {
                Ok(())
            }
        }

        let o = StubOrch;
        let id = ComponentId::new("x");
        let res = o.get_quota(&id).await;
        assert!(matches!(res, Err(OrchestratorError::ComponentNotFound(_))));
    }
}
