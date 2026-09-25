//! `CgroupQuota` —— 基于 cgroups-rs 的真实 cgroup v2 资源配额实现
//!
//! 定位（规格书 §3 关键实现 / ADR-DEPS-002 已注册 `cgroups-rs 0.5`）：
//! - 把 `ResourceQuota`（CPU 核数 / 内存字节 / IO 带宽）翻译成 cgroup v2 的
//!   `cpu.max` / `memory.max` / `io.max` 写入，由 cgroups-rs 真实落地。
//! - `set_quota` 在线调整无需重启进程。
//!
//! ## 权限与可测试性（规格书 §9 红线 / §6 硬阻塞）
//! 真实 cgroup v2 写入需要 **root + CAP_SYS_ADMIN + cgroup v2 挂载**——
//! 在沙箱外运行会污染宿主或失败（EPERM）。为支持"不依赖 root 的单元测试"，
//! 本模块把 cgroup 操作抽象成 [`CgroupBackend`] trait：
//!
//! | 后端 | 用途 | 真实写 cgroup？ |
//! |------|------|---------------|
//! | [`CgroupsRsBackend`] | 生产（root 沙箱） | ✅ 写 `/sys/fs/cgroup/<base>/<id>` |
//! | [`InMemoryCgroupBackend`] | 单元测试/fixture | ❌ 仅内存哈希 |
//!
//! [`SystemdOrchestrator`](crate::SystemdOrchestrator) 默认注入 `CgroupsRsBackend`，
//! 测试构造时注入 `InMemoryCgroupBackend`，避免真写 cgroup（红线）。
//!
//! ## 配额 → cgroup v2 文件映射
//! | `ResourceQuota` 字段 | cgroup v2 文件 | 转换 |
//! |---------------------|---------------|------|
//! | `cpu_cores: Some(c)` | `cpu.max = "<c*100000> 100000"` | CFS：100ms 周期内允许 `c*100000` 微秒 |
//! | `cpu_cores: None`    | `cpu.max = "max 100000"`         | 不限 |
//! | `memory_bytes: Some(b)` | `memory.max = "<b>"` | 字节硬上限 |
//! | `memory_bytes: None`    | `memory.max = "max"`  | 不限 |
//! | `io_bps_limit` | （暂只记录快照，见 [`CgroupsRsBackend`] 注释） | 需设备主次号 |
//!
//! cgroup v2 `io.max` 需 `<major>:<minor> rbps=<bytes>`，但 `ResourceQuota`
//! 不携带设备号；故 IO 限制当前**仅写入快照**（写入 IO 限速留待扩展
//! `ResourceQuota` 加 `device` 字段后补，ADR 走 trait 签名修订流程）。

use std::collections::HashMap;
use std::sync::Mutex;

use os_core::ResourceQuota;

use crate::ComponentId;
use crate::OrchestratorError;

/// cgroup v2 默认 CFS 周期（微秒）= 100ms（内核默认值，见 `cpu.max` 文档）
const CGROUP_V2_CFS_PERIOD_US: u64 = 100_000;

/// cgroup 操作后端抽象
///
/// 抽象 cgroup v2 的"写配额/读配额"操作，便于单元测试用内存后端替身，
/// 避免真写 `/sys/fs/cgroup`（需 root，规格书 §9 红线）。
///
/// 实现者必须线程安全（`Send + Sync`）；`SystemdOrchestrator` 跨多组件并发调用。
pub trait CgroupBackend: Send + Sync {
    /// 把 `quota` 写入 `<base>/<component_id>` 对应的 cgroup
    ///
    /// 后端负责：创建（或更新）cgroup 目录、把 `ResourceQuota` 翻译成
    /// `cpu.max`/`memory.max`/`io.max` 写入。失败返回 [`OrchestratorError::QuotaFailed`]。
    fn apply_quota(
        &self,
        base: &str,
        component_id: &ComponentId,
        quota: &ResourceQuota,
    ) -> Result<(), OrchestratorError>;

    /// 读 `<base>/<component_id>` cgroup 当前配额（best-effort）
    ///
    /// 后端尽力把 cgroup 文件读回翻译成 `ResourceQuota`；某些字段无法回读时
    /// （如 IO 无设备号）返回 `None` 字段。cgroup 不存在时返回 `Ok(None)`。
    fn read_quota(
        &self,
        base: &str,
        component_id: &ComponentId,
    ) -> Result<Option<ResourceQuota>, OrchestratorError>;
}

// ----------------------------------------------------------------------------
// 真实后端：cgroups-rs（生产用，需 root + cgroup v2）
// ----------------------------------------------------------------------------

/// 基于 `cgroups-rs` 的真实 cgroup v2 后端
///
/// 写入路径：`/sys/fs/cgroup/<base>/<component_id>`（cgroup v2 unified 挂载点）。
/// `base` 默认 `"os"`，所有 OS 业务组件的 cgroup 集中在 `/sys/fs/cgroup/os/` 下，
/// 便于整体限速/查询。
///
/// **权限**：所有写操作需 root + CAP_SYS_ADMIN（规格书 §6 / §8）。
#[derive(Debug, Default, Clone)]
pub struct CgroupsRsBackend {
    /// cgroup v2 是否可用（惰性探测，避免构造时 IO）
    /// 缓存探测结果：true=已确认 v2，false=已确认非 v2，None=未探测
    v2_known: std::sync::OnceLock<bool>,
}

impl CgroupsRsBackend {
    /// 构造
    pub fn new() -> Self {
        Self::default()
    }

    /// 探测当前系统是否为 cgroup v2 unified 模式
    ///
    /// 缓存结果（`OnceLock`）；多次调用零额外开销。
    fn is_v2(&self) -> bool {
        *self
            .v2_known
            .get_or_init(cgroups_rs::fs::hierarchies::is_cgroup2_unified_mode)
    }

    /// 把 `ResourceQuota` 翻译成 cgroups-rs 的 `Resources` 结构
    fn quota_to_resources(quota: &ResourceQuota) -> cgroups_rs::fs::Resources {
        use cgroups_rs::fs::{CpuResources, MemoryResources, Resources};

        // CPU：CFS 配额 = 核数 × 周期（100ms）；None → max（不限）
        let (cpu_quota, cpu_period) = match quota.cpu_cores {
            Some(cores) if cores > 0.0 => {
                // cores * 100000 微秒，四舍五入为正整数
                let us = (cores * CGROUP_V2_CFS_PERIOD_US as f32).round() as i64;
                (Some(us.max(1)), Some(CGROUP_V2_CFS_PERIOD_US))
            }
            _ => (None, Some(CGROUP_V2_CFS_PERIOD_US)), // None/0 → max
        };

        // 内存：硬上限字节；None → max
        // （MemoryResources.memory_hard_limit 是 Option<i64>；负值在 cgroup v2 表示 max）
        let mem_hard_limit = quota.memory_bytes.map(|b| b as i64);

        Resources {
            memory: MemoryResources {
                memory_hard_limit: mem_hard_limit,
                ..Default::default()
            },
            cpu: CpuResources {
                quota: cpu_quota,
                period: cpu_period,
                ..Default::default()
            },
            // io.max 需设备号，ResourceQuota 暂不携带；attrs 通道留给将来扩展
            // （写 io.max 走 blkio.attrs，见 cgroups-rs 文档示例）
            ..Default::default()
        }
    }

    /// 从 cgroups-rs `Resources` 反推 `ResourceQuota`（读 cgroup 文件后用）
    fn resources_to_quota(res: &cgroups_rs::fs::Resources) -> ResourceQuota {
        // CPU：quota+period → 核数；max → None
        let cpu_cores = match (res.cpu.quota, res.cpu.period) {
            (Some(q), Some(p)) if q > 0 && p > 0 => Some((q as f32) / (p as f32)),
            _ => None,
        };
        // 内存：硬上限 i64 → u64；负值或 None 视为 max（不限）
        let memory_bytes = match res.memory.memory_hard_limit {
            Some(b) if b >= 0 => Some(b as u64),
            _ => None,
        };
        ResourceQuota {
            cpu_cores,
            memory_bytes,
            // io_bps_limit 无法从 io.max 无设备号地反推；保持 None
            io_bps_limit: None,
        }
    }
}

impl CgroupBackend for CgroupsRsBackend {
    fn apply_quota(
        &self,
        base: &str,
        component_id: &ComponentId,
        quota: &ResourceQuota,
    ) -> Result<(), OrchestratorError> {
        if !self.is_v2() {
            return Err(OrchestratorError::QuotaFailed(format!(
                "当前系统非 cgroup v2 unified 模式，无法为组件 {} 写配额（osd 仅支持 v2）",
                component_id
            )));
        }

        // cgroup 路径：/sys/fs/cgroup/<base>/<component_id>
        let path = format!("{}/{}", base, component_id);
        let hier = Box::new(cgroups_rs::fs::hierarchies::V2::new());

        // 先尝试 load 已存在的 cgroup；不存在则 new（create）
        let cg = cgroups_rs::fs::Cgroup::load(hier.clone(), &path);
        let cg = if cg.exists() {
            cg
        } else {
            // Cgroup::new 内部会 create
            cgroups_rs::fs::Cgroup::new(hier, &path).map_err(|e| {
                OrchestratorError::QuotaFailed(format!(
                    "创建 cgroup {} 失败: {}（确认以 root 运行且 cgroup v2 可写）",
                    path, e
                ))
            })?
        };

        let res = Self::quota_to_resources(quota);
        cg.apply(&res).map_err(|e| {
            OrchestratorError::QuotaFailed(format!("写入 cgroup {} 配额失败: {}", path, e))
        })
    }

    fn read_quota(
        &self,
        base: &str,
        component_id: &ComponentId,
    ) -> Result<Option<ResourceQuota>, OrchestratorError> {
        if !self.is_v2() {
            // 非 v2 视为"读不到"，调用方回退到快照
            return Ok(None);
        }

        let path = format!("{}/{}", base, component_id);
        let hier = Box::new(cgroups_rs::fs::hierarchies::V2::new());
        let cg = cgroups_rs::fs::Cgroup::load(hier, &path);
        if !cg.exists() {
            return Ok(None);
        }

        // 从 cpu.max / memory.max 读回
        let mut res = cgroups_rs::fs::Resources::default();

        if let Some(cpu_ctl) = cg.controller_of::<cgroups_rs::fs::cpu::CpuController>() {
            // CFS quota/period
            if let Ok(q) = cpu_ctl.cfs_quota() {
                res.cpu.quota = Some(q);
            }
            if let Ok(p) = cpu_ctl.cfs_period() {
                res.cpu.period = Some(p);
            }
        }
        if let Some(mem_ctl) = cg.controller_of::<cgroups_rs::fs::memory::MemController>() {
            if let Ok(set) = mem_ctl.get_mem() {
                // set.max 是 Option<MaxValue>；转回 i64（Max → -1，表示"不限"）
                res.memory.memory_hard_limit = set.max.map(|mv| match mv {
                    cgroups_rs::fs::MaxValue::Max => -1,
                    cgroups_rs::fs::MaxValue::Value(v) => v,
                });
            }
        }

        Ok(Some(Self::resources_to_quota(&res)))
    }
}

// ----------------------------------------------------------------------------
// 测试后端：内存哈希（不写 cgroup，零 root 依赖）
// ----------------------------------------------------------------------------

/// 内存 cgroup 后端（仅测试 / feature gate `mock` 下用）
///
/// 行为：把 `ResourceQuota` 存在 `HashMap<(base, id), ResourceQuota>`，
/// `read_quota` 原样读回。**不**写任何文件，可在非 root 沙箱运行。
///
/// 用法见 `cgroup::tests` 与 `impl_orchestrator::tests`（注入 `SystemdOrchestrator`）。
#[derive(Debug, Default)]
pub struct InMemoryCgroupBackend {
    store: Mutex<HashMap<(String, String), ResourceQuota>>,
}

impl InMemoryCgroupBackend {
    /// 构造空后端
    pub fn new() -> Self {
        Self::default()
    }
}

impl CgroupBackend for InMemoryCgroupBackend {
    fn apply_quota(
        &self,
        base: &str,
        component_id: &ComponentId,
        quota: &ResourceQuota,
    ) -> Result<(), OrchestratorError> {
        let key = (base.to_string(), component_id.to_string());
        self.store
            .lock()
            .expect("InMemoryCgroupBackend store poisoned")
            .insert(key, quota.clone());
        Ok(())
    }

    fn read_quota(
        &self,
        base: &str,
        component_id: &ComponentId,
    ) -> Result<Option<ResourceQuota>, OrchestratorError> {
        let key = (base.to_string(), component_id.to_string());
        Ok(self
            .store
            .lock()
            .expect("InMemoryCgroupBackend store poisoned")
            .get(&key)
            .cloned())
    }
}

// ----------------------------------------------------------------------------
// CgroupQuota：组件 ID → cgroup 路径 + 后端委派 + 快照缓存
// ----------------------------------------------------------------------------

/// cgroup v2 配额管理器（`SystemdOrchestrator` 的配额子系统）
///
/// 职责：
/// - 维护 `base`（cgroup 根前缀，默认 `"os"`）。
/// - 持有一个 [`CgroupBackend`]（生产用 [`CgroupsRsBackend`]，测试注入
///   [`InMemoryCgroupBackend`]）。
/// - 缓存最近一次成功写入的配额快照（`HashMap<ComponentId, ResourceQuota>`），
///   供 `get_quota` 在后端不可读（如非 root 环境）时回退返回。
///
/// 线程安全：内部状态用 `RwLock`/`Mutex` 保护，可被 `SystemdOrchestrator` 跨
/// 组件并发调用（与同组件串行化锁正交——本类型自身线程安全即可）。
pub struct CgroupQuota {
    base: String,
    backend: Box<dyn CgroupBackend>,
    snapshots: Mutex<HashMap<ComponentId, ResourceQuota>>,
}

impl std::fmt::Debug for CgroupQuota {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CgroupQuota")
            .field("base", &self.base)
            .field("backend", &"<dyn CgroupBackend>")
            .field(
                "snapshots_len",
                &self.snapshots.lock().map(|s| s.len()).unwrap_or(0),
            )
            .finish()
    }
}

impl CgroupQuota {
    /// 用真实 cgroups-rs 后端构造（生产用，需 root）
    ///
    /// `base` 为 cgroup 根前缀，默认 `"os"`（即所有组件 cgroup 在
    /// `/sys/fs/cgroup/os/<component_id>` 下）。
    pub fn new(base: impl Into<String>) -> Self {
        Self::with_backend(base, Box::new(CgroupsRsBackend::new()))
    }

    /// 用自定义后端构造（测试用，注入 `InMemoryCgroupBackend`）
    pub fn with_backend(base: impl Into<String>, backend: Box<dyn CgroupBackend>) -> Self {
        Self {
            base: base.into(),
            backend,
            snapshots: Mutex::new(HashMap::new()),
        }
    }

    /// 取 base 前缀
    pub fn base(&self) -> &str {
        &self.base
    }

    /// 设置组件配额：写 cgroup（后端） + 更新快照
    ///
    /// 失败映射为 [`OrchestratorError::QuotaFailed`]；快照只在写入成功后更新。
    pub fn set_quota(
        &self,
        component_id: &ComponentId,
        quota: &ResourceQuota,
    ) -> Result<(), OrchestratorError> {
        self.backend.apply_quota(&self.base, component_id, quota)?;
        // 写入成功后更新快照（get_quota 在后端不可读时回退用）
        self.snapshots
            .lock()
            .expect("snapshots poisoned")
            .insert(component_id.clone(), quota.clone());
        Ok(())
    }

    /// 取组件配额：优先读后端；后端读不到（None）则回退到快照
    pub fn get_quota(
        &self,
        component_id: &ComponentId,
    ) -> Result<Option<ResourceQuota>, OrchestratorError> {
        if let Some(q) = self.backend.read_quota(&self.base, component_id)? {
            return Ok(Some(q));
        }
        Ok(self
            .snapshots
            .lock()
            .expect("snapshots poisoned")
            .get(component_id)
            .cloned())
    }
}

// ----------------------------------------------------------------------------
// tests
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn q(cpu: Option<f32>, mem: Option<u64>, io: Option<u64>) -> ResourceQuota {
        ResourceQuota {
            cpu_cores: cpu,
            memory_bytes: mem,
            io_bps_limit: io,
        }
    }

    fn id(s: &str) -> ComponentId {
        ComponentId::new(s)
    }

    // ---- quota_to_resources / resources_to_quota 往返（纯函数，不碰 cgroup） ----

    #[test]
    fn quota_to_resources_cpu_cores_translates_to_cfs_quota() {
        let quota = q(Some(2.0), None, None);
        let res = CgroupsRsBackend::quota_to_resources(&quota);
        // 2 核 × 100000 = 200000us quota，period=100000
        assert_eq!(res.cpu.quota, Some(200_000));
        assert_eq!(res.cpu.period, Some(CGROUP_V2_CFS_PERIOD_US));
    }

    #[test]
    fn quota_to_resources_half_core_rounds() {
        let quota = q(Some(0.5), None, None);
        let res = CgroupsRsBackend::quota_to_resources(&quota);
        assert_eq!(res.cpu.quota, Some(50_000));
    }

    #[test]
    fn quota_to_resources_no_cpu_means_no_quota_field() {
        let quota = q(None, None, None);
        let res = CgroupsRsBackend::quota_to_resources(&quota);
        // None CPU → quota None（写 cpu.max 时 cgroups-rs 视为 max）
        assert_eq!(res.cpu.quota, None);
        assert_eq!(res.cpu.period, Some(CGROUP_V2_CFS_PERIOD_US));
    }

    #[test]
    fn quota_to_resources_zero_cpu_treated_as_unlimited() {
        // cpu_cores = Some(0.0) 无意义，等同 None
        let quota = q(Some(0.0), None, None);
        let res = CgroupsRsBackend::quota_to_resources(&quota);
        assert_eq!(res.cpu.quota, None);
    }

    #[test]
    fn quota_to_resources_memory_translates_to_hard_limit() {
        let quota = q(None, Some(512 * 1024 * 1024), None);
        let res = CgroupsRsBackend::quota_to_resources(&quota);
        assert_eq!(res.memory.memory_hard_limit, Some(512 * 1024 * 1024));
    }

    #[test]
    fn quota_to_resources_no_memory_means_no_limit() {
        let quota = q(None, None, None);
        let res = CgroupsRsBackend::quota_to_resources(&quota);
        assert_eq!(res.memory.memory_hard_limit, None);
    }

    #[test]
    fn resources_to_quota_roundtrip_cpu() {
        // quota → resources → quota（CPU 核数往返）
        let original = q(Some(1.5), None, None);
        let res = CgroupsRsBackend::quota_to_resources(&original);
        let back = CgroupsRsBackend::resources_to_quota(&res);
        assert_eq!(back.cpu_cores, Some(1.5));
    }

    #[test]
    fn resources_to_quota_roundtrip_memory() {
        let original = q(None, Some(1_000_000), None);
        let res = CgroupsRsBackend::quota_to_resources(&original);
        let back = CgroupsRsBackend::resources_to_quota(&res);
        assert_eq!(back.memory_bytes, Some(1_000_000));
    }

    // ---- InMemoryCgroupBackend（不写 cgroup） ----

    #[test]
    fn in_memory_backend_applies_and_reads_back() {
        let b = InMemoryCgroupBackend::new();
        let quota = q(Some(2.0), Some(1_000), None);
        b.apply_quota("os", &id("comp-a"), &quota).unwrap();
        let got = b.read_quota("os", &id("comp-a")).unwrap().unwrap();
        assert_eq!(got.cpu_cores, Some(2.0));
        assert_eq!(got.memory_bytes, Some(1_000));
    }

    #[test]
    fn in_memory_backend_read_unknown_returns_none() {
        let b = InMemoryCgroupBackend::new();
        assert!(b.read_quota("os", &id("missing")).unwrap().is_none());
    }

    #[test]
    fn in_memory_backend_apply_overwrites() {
        let b = InMemoryCgroupBackend::new();
        b.apply_quota("os", &id("c"), &q(Some(1.0), None, None))
            .unwrap();
        b.apply_quota("os", &id("c"), &q(Some(3.0), None, None))
            .unwrap();
        let got = b.read_quota("os", &id("c")).unwrap().unwrap();
        assert_eq!(got.cpu_cores, Some(3.0));
    }

    #[test]
    fn in_memory_backend_base_isolation() {
        // 不同 base 下同名组件互不干扰
        let b = InMemoryCgroupBackend::new();
        b.apply_quota("os", &id("c"), &q(Some(1.0), None, None))
            .unwrap();
        b.apply_quota("test", &id("c"), &q(Some(2.0), None, None))
            .unwrap();
        assert_eq!(
            b.read_quota("os", &id("c")).unwrap().unwrap().cpu_cores,
            Some(1.0)
        );
        assert_eq!(
            b.read_quota("test", &id("c")).unwrap().unwrap().cpu_cores,
            Some(2.0)
        );
    }

    // ---- CgroupQuota（用 InMemory 后端，不写 cgroup） ----

    #[test]
    fn cgroup_quota_set_then_get_returns_snapshot() {
        let cq = CgroupQuota::with_backend("os", Box::new(InMemoryCgroupBackend::new()));
        let quota = q(Some(1.0), Some(2_000), None);
        cq.set_quota(&id("c"), &quota).unwrap();
        let got = cq.get_quota(&id("c")).unwrap().unwrap();
        assert_eq!(got.cpu_cores, Some(1.0));
        assert_eq!(got.memory_bytes, Some(2_000));
    }

    #[test]
    fn cgroup_quota_get_unknown_returns_none() {
        let cq = CgroupQuota::with_backend("os", Box::new(InMemoryCgroupBackend::new()));
        assert!(cq.get_quota(&id("missing")).unwrap().is_none());
    }

    #[test]
    fn cgroup_quota_base_default_is_os() {
        let cq = CgroupQuota::with_backend("os", Box::new(InMemoryCgroupBackend::new()));
        assert_eq!(cq.base(), "os");
    }

    // ---- CgroupsRsBackend 在非 root / 非 v2 环境的降级（沙箱测试） ----
    //
    // 说明：CI / 单元测环境通常非 cgroup v2 unified 或非 root，以下测验证后端
    // 在这种环境下返回结构化错误而非 panic（规格 §9 红线：不污染宿主）。

    #[test]
    fn real_backend_apply_in_non_v2_env_returns_quota_failed_not_panic() {
        let b = CgroupsRsBackend::new();
        let res = b.apply_quota("os", &id("c"), &q(Some(1.0), None, None));
        // 当前测环境是 cgroup v2 unified（多数现代 Linux），则写入会因非 root 失败；
        // 若非 v2，则返回 QuotaFailed"非 v2"。两者都应映射为 QuotaFailed，不 panic。
        match res {
            Err(OrchestratorError::QuotaFailed(msg)) => {
                // 错误信息应提到组件或权限
                assert!(
                    msg.contains("cgroup") || msg.contains("组件") || msg.contains("root"),
                    "错误信息应提及 cgroup/组件/root，实际: {msg}"
                );
            }
            Ok(()) => {
                // 极端情况：当前进程有写权限且 v2 可写——也算通过（说明在容器内 root）
            }
            other => panic!("期望 QuotaFailed 或 Ok，实际: {other:?}"),
        }
    }

    #[test]
    fn real_backend_read_in_non_v2_or_non_root_returns_ok_none_or_quota() {
        // read_quota 不应 panic；返回 Ok(None) 或 Ok(Some) 均可
        let b = CgroupsRsBackend::new();
        let res = b.read_quota("os", &id("nonexistent-component-xyz"));
        assert!(res.is_ok(), "read_quota 不应返回 Err: {res:?}");
    }

    // ---- CgroupQuota：快照回退路径（后端读不到 → 用快照） ----
    //
    // 用一个"读总是返回 None"的后端，验证 get_quota 在后端不可读时回退到 set_quota
    // 写入的快照。这覆盖 snapshots HashMap 的真实使用路径。

    /// 永远读 None 的后端（模拟非 root 环境下 cgroup 不可读）
    struct AlwaysNoneBackend;
    impl CgroupBackend for AlwaysNoneBackend {
        fn apply_quota(
            &self,
            _base: &str,
            _component_id: &ComponentId,
            _quota: &ResourceQuota,
        ) -> Result<(), OrchestratorError> {
            Ok(())
        }
        fn read_quota(
            &self,
            _base: &str,
            _component_id: &ComponentId,
        ) -> Result<Option<ResourceQuota>, OrchestratorError> {
            Ok(None)
        }
    }

    #[test]
    fn cgroup_quota_falls_back_to_snapshot_when_backend_reads_none() {
        let cq = CgroupQuota::with_backend("os", Box::new(AlwaysNoneBackend));
        let quota = q(Some(4.0), Some(8_192), None);
        cq.set_quota(&id("c"), &quota).unwrap();
        // 后端读 None → 回退到快照
        let got = cq.get_quota(&id("c")).unwrap().expect("快照应有值");
        assert_eq!(got.cpu_cores, Some(4.0));
        assert_eq!(got.memory_bytes, Some(8_192));
    }

    #[test]
    fn cgroup_quota_set_overwrites_snapshot() {
        let cq = CgroupQuota::with_backend("os", Box::new(AlwaysNoneBackend));
        cq.set_quota(&id("c"), &q(Some(1.0), None, None)).unwrap();
        cq.set_quota(&id("c"), &q(Some(9.0), None, None)).unwrap();
        let got = cq.get_quota(&id("c")).unwrap().unwrap();
        assert_eq!(got.cpu_cores, Some(9.0));
    }

    #[test]
    fn cgroup_quota_snapshot_isolated_per_component() {
        let cq = CgroupQuota::with_backend("os", Box::new(AlwaysNoneBackend));
        cq.set_quota(&id("a"), &q(Some(1.0), None, None)).unwrap();
        cq.set_quota(&id("b"), &q(Some(2.0), None, None)).unwrap();
        assert_eq!(
            cq.get_quota(&id("a")).unwrap().unwrap().cpu_cores,
            Some(1.0)
        );
        assert_eq!(
            cq.get_quota(&id("b")).unwrap().unwrap().cpu_cores,
            Some(2.0)
        );
    }

    #[test]
    fn cgroup_quota_new_real_backend_default_base_os() {
        // 生产构造路径：CgroupQuota::new(base) → 内部 CgroupsRsBackend
        let cq = CgroupQuota::new("os");
        assert_eq!(cq.base(), "os");
    }

    #[test]
    fn cgroup_quota_base_custom_prefix() {
        let cq = CgroupQuota::with_backend("custom-base", Box::new(InMemoryCgroupBackend::new()));
        assert_eq!(cq.base(), "custom-base");
    }

    // ---- CgroupQuota Debug（覆盖自定义 Debug impl） ----

    #[test]
    fn cgroup_quota_debug_includes_base_and_snapshot_len() {
        let cq = CgroupQuota::with_backend("dbg-base", Box::new(InMemoryCgroupBackend::new()));
        cq.set_quota(&id("c1"), &q(Some(1.0), None, None)).unwrap();
        cq.set_quota(&id("c2"), &q(Some(2.0), None, None)).unwrap();
        let s = format!("{cq:?}");
        assert!(s.contains("dbg-base"), "Debug 应含 base：{s}");
        assert!(s.contains("snapshots_len"), "Debug 应含 snapshots_len：{s}");
        assert!(s.contains("2"), "snapshots_len 应为 2：{s}");
    }

    // ---- InMemoryCgroupBackend 边界 ----

    #[test]
    fn in_memory_backend_default_is_empty() {
        let b = InMemoryCgroupBackend::default();
        assert!(b.read_quota("any", &id("any")).unwrap().is_none());
    }

    #[test]
    fn in_memory_backend_isolated_per_id_under_same_base() {
        let b = InMemoryCgroupBackend::new();
        b.apply_quota("os", &id("a"), &q(Some(1.0), None, None))
            .unwrap();
        b.apply_quota("os", &id("b"), &q(Some(2.0), None, None))
            .unwrap();
        assert_eq!(
            b.read_quota("os", &id("a")).unwrap().unwrap().cpu_cores,
            Some(1.0)
        );
        assert_eq!(
            b.read_quota("os", &id("b")).unwrap().unwrap().cpu_cores,
            Some(2.0)
        );
    }

    #[test]
    fn in_memory_backend_full_quota_roundtrip_with_io() {
        let b = InMemoryCgroupBackend::new();
        let quota = q(Some(0.5), Some(1_073_741_824), Some(104_857_600));
        b.apply_quota("os", &id("full"), &quota).unwrap();
        let got = b.read_quota("os", &id("full")).unwrap().unwrap();
        assert_eq!(got.cpu_cores, Some(0.5));
        assert_eq!(got.memory_bytes, Some(1_073_741_824));
        assert_eq!(got.io_bps_limit, Some(104_857_600));
    }

    // ---- quota_to_resources 边界 ----

    #[test]
    fn quota_to_resources_negative_cpu_treated_as_unlimited() {
        // cpu_cores = Some(-1.0) 无意义 → match _ 分支 → None（max）
        let quota = q(Some(-1.0), None, None);
        let res = CgroupsRsBackend::quota_to_resources(&quota);
        assert_eq!(res.cpu.quota, None);
    }

    #[test]
    fn quota_to_resources_full_quota_all_fields() {
        let quota = q(Some(2.0), Some(2_000_000_000), Some(123));
        let res = CgroupsRsBackend::quota_to_resources(&quota);
        assert_eq!(res.cpu.quota, Some(200_000));
        assert_eq!(res.cpu.period, Some(CGROUP_V2_CFS_PERIOD_US));
        assert_eq!(res.memory.memory_hard_limit, Some(2_000_000_000));
        // io_bps_limit 不映射到 cgroups-rs Resources（无设备号），故 resources 中无对应字段
    }

    // ---- resources_to_quota 边界（覆盖 None 与负值分支） ----

    #[test]
    fn resources_to_quota_cpu_none_period_none_yields_none() {
        let res = cgroups_rs::fs::Resources::default();
        let back = CgroupsRsBackend::resources_to_quota(&res);
        assert_eq!(back.cpu_cores, None);
        assert_eq!(back.memory_bytes, None);
        assert_eq!(back.io_bps_limit, None);
    }

    #[test]
    fn resources_to_quota_memory_negative_yields_none() {
        // memory_hard_limit 为负（cgroup v2 表示 max）→ None
        let mut res = cgroups_rs::fs::Resources::default();
        res.memory.memory_hard_limit = Some(-1);
        let back = CgroupsRsBackend::resources_to_quota(&res);
        assert_eq!(back.memory_bytes, None);
    }

    #[test]
    fn resources_to_quota_cpu_zero_quota_yields_none() {
        // quota=0 或 period=0 → None（避免除零）
        let mut res = cgroups_rs::fs::Resources::default();
        res.cpu.quota = Some(0);
        res.cpu.period = Some(100_000);
        let back = CgroupsRsBackend::resources_to_quota(&res);
        assert_eq!(back.cpu_cores, None);
    }

    #[test]
    fn resources_to_quota_memory_zero_is_valid() {
        // memory_hard_limit = Some(0) 是合法值（0 字节硬上限）→ Some(0)
        let mut res = cgroups_rs::fs::Resources::default();
        res.memory.memory_hard_limit = Some(0);
        let back = CgroupsRsBackend::resources_to_quota(&res);
        assert_eq!(back.memory_bytes, Some(0));
    }

    // ---- quota ↔ resources 完整往返（CPU + 内存同存） ----

    #[test]
    fn quota_resources_full_roundtrip_cpu_and_memory() {
        let original = q(Some(1.5), Some(5_000_000), None);
        let res = CgroupsRsBackend::quota_to_resources(&original);
        let back = CgroupsRsBackend::resources_to_quota(&res);
        assert_eq!(back.cpu_cores, Some(1.5));
        assert_eq!(back.memory_bytes, Some(5_000_000));
        assert_eq!(back.io_bps_limit, None);
    }
}
