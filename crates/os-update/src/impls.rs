//! 默认实现——A/B 双槽 / 回滚 / CVE / 滚动升级（规划文档 §3.12）。
//!
//! 本模块为四个 trait 提供**默认实现 struct**，命名按规格书 §5.1：
//! - [`AbUpdateEngine`]：`impl UpdateEngine`，A/B 双槽位编排。
//! - [`AbRollbackManager`]：`impl RollbackManager`，配合 watchdog 自动回滚。
//! - [`NvdCveMonitor`]：`impl CveMonitor`，对接 NVD/OSV 数据源。
//! - [`HaRollingUpgrade`]：`impl RollingUpgrade`，配合 leader 选举。
//!
//! **真实 I/O 接通状态**：
//! - ✅ `AbUpdateEngine.check_updates` / `download` / `verify`：reqwest 拉清单 +
//!   reqwest 下载到暂存盘 + ed25519 验签 + sha256 比对（见 [`crate::real`]）。
//! - ✅ `NvdCveMonitor.check_advisories`：reqwest POST OSV `/query` 批量查询，
//!   [`crate::real::parse_osv_advisories`] 解析过滤。
//! - ✅ `AbUpdateEngine.activate_slot`：真实 bootloader 编排（GRUB/systemd-boot），
//!   经 [`crate::bootloader::BootloaderRunner`] 抽象调用 `grub2-reboot`/
//!   `bootctl set-oneshot`（next-boot 一次性切换，失败可回滚）。
//! - ⏳ `AbRollbackManager.verify_current_health`：探针实现待健康检查依赖注册。
//! - ⏳ `HaRollingUpgrade.execute`：逐节点升级编排待 meta leader 选举。
//!
//! 安全红线：`verify` 一律真实验签（[`crate::real::verify_package`]），不可绕过；
//! 激活路径（`activate_slot`）在调用前必须确认 `verify` 已通过（由调用方在更高层把关）。
//!
//! 复用纯逻辑（[`crate::slot::SlotManager`] / [`crate::rolling::RollingStateMachine`] /
//! [`crate::rollback::should_rollback`]）封装决策路径，无 bootloader 依赖即可测。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use os_core::{HealthReport, NodeId, NodeInfo, TaskId};

use crate::real::{download_to_file, parse_osv_advisories, verify_package};
use crate::rollback::{
    should_rollback, RollbackContext, RollbackDecision, RollbackManager, RollbackPoint,
    RollbackPolicy,
};
use crate::rolling::{
    decide_upgrade_order, RollingPlan, RollingStateMachine, RollingStatus, RollingStrategy,
    RollingUpgrade,
};
use crate::slot::{SlotManager, SlotStatus};
use crate::update::{UpdateEngine, UpdateManifest, UpdateSlot, UpdateStatus};
use crate::CveAdvisory;
use crate::{CveCallback, CveMonitor};

// ============================================================================
// AbUpdateEngine —— A/B 双槽位 OTA 编排
// ============================================================================

/// A/B 双槽位更新引擎（默认实现）。
///
/// 持有 [`SlotManager`]（纯状态机）+ 任务状态表 + reqwest 客户端 + 更新源 URL +
/// 可信 ed25519 公钥 + bootloader 配置 + bootloader 执行器。`download`/`verify`/
/// `check_updates` 走真实 reqwest + ed25519/sha256（已接通）；`activate_slot` 走
/// 真实 bootloader 编排（GRUB/systemd-boot，经 [`crate::bootloader::BootloaderRunner`] 抽象，已接通）。
///
/// 槽位决策（写入目标 / 激活切换 / 失败回滚）复用 [`SlotManager`]；bootloader
/// 工具调用复用 [`crate::bootloader`] 的两阶段编排（next-boot 一次性 → 探活 → commit）。
pub struct AbUpdateEngine {
    /// A/B 双槽状态机
    slot: Mutex<SlotManager>,
    /// 任务状态表（TaskId → UpdateStatus）
    tasks: Mutex<HashMap<TaskId, UpdateStatus>>,
    /// reqwest 客户端（下载 + 清单拉取）
    client: reqwest::Client,
    /// 更新源根 URL（清单与包均相对此 URL 解析）
    update_source: String,
    /// 下载暂存目录（包落盘于此，校验后再写槽）
    staging_dir: PathBuf,
    /// 可信 ed25519 公钥（32 字节，构建期烧录）
    pubkey: [u8; 32],
    /// bootloader 配置（A/B 槽 entry + default + next_default）
    bootloader: Mutex<crate::bootloader::BootloaderConfig>,
    /// bootloader 工具执行器（默认 TokioBootloaderRunner，测试可注入 fixture）
    runner: Box<dyn crate::bootloader::BootloaderRunner>,
}

impl AbUpdateEngine {
    /// 构造：给定当前活动槽 + 版本 + 更新源 URL + 可信公钥 + bootloader 类型。
    ///
    /// bootloader 配置按默认 entry 模板生成（slot A/B 的 kernel/initrd 路径按
    /// `/boot/slot-<a|b>/vmlinuz` 约定）；如需自定义 entry 见 [`Self::with_bootloader`]。
    /// 下载暂存目录默认为 `/tmp/os-update-staging`；测试可用 [`Self::with_staging_dir`]。
    /// bootloader 执行器默认用 [`crate::bootloader::TokioBootloaderRunner`]（spawn 真实子进程），
    /// 测试可用 [`Self::with_runner`] 注入 fixture。
    #[must_use]
    pub fn new(
        active_slot: UpdateSlot,
        current_version: impl Into<String>,
        update_source: impl Into<String>,
        pubkey: [u8; 32],
        bootloader_kind: crate::bootloader::BootloaderKind,
    ) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("os-update/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client 构造失败");
        let current_version = current_version.into();
        let bootloader = default_bootloader_config(bootloader_kind, active_slot, &current_version);
        Self {
            slot: Mutex::new(SlotManager::new(
                active_slot,
                &current_version,
                chrono::Utc::now(),
            )),
            tasks: Mutex::new(HashMap::new()),
            client,
            update_source: update_source.into(),
            staging_dir: PathBuf::from("/tmp/os-update-staging"),
            pubkey,
            bootloader: Mutex::new(bootloader),
            runner: Box::new(crate::bootloader::TokioBootloaderRunner),
        }
    }

    /// 测试/部署覆盖下载暂存目录。
    #[must_use]
    pub fn with_staging_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.staging_dir = dir.into();
        self
    }

    /// 注入自定义 reqwest 客户端（用于 fixture 测试改写 base_url/代理等）。
    #[must_use]
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// 覆盖 bootloader 配置（自定义 entry 路径/cmdline）。
    #[must_use]
    pub fn with_bootloader(self, cfg: crate::bootloader::BootloaderConfig) -> Self {
        *self.bootloader.lock().expect("bootloader poisoned") = cfg;
        self
    }

    /// 注入自定义 bootloader 执行器（测试用 fixture 替代真实子进程）。
    #[must_use]
    pub fn with_runner(mut self, runner: Box<dyn crate::bootloader::BootloaderRunner>) -> Self {
        self.runner = runner;
        self
    }

    /// 取槽位状态机快照（供测试/诊断）。
    pub fn slot_manager(&self) -> SlotManager {
        self.slot.lock().expect("slot poisoned").clone()
    }

    /// 取 bootloader 配置快照（供测试/诊断）。
    pub fn bootloader_config(&self) -> crate::bootloader::BootloaderConfig {
        self.bootloader.lock().expect("bootloader poisoned").clone()
    }

    /// 更新源根 URL（只读，供测试断言）。
    pub fn update_source(&self) -> &str {
        &self.update_source
    }
}

/// 构造默认 bootloader 配置：两槽 entry 按 `/boot/slot-<a|b>/vmlinuz` 约定，
/// default = 当前 active 槽。
fn default_bootloader_config(
    kind: crate::bootloader::BootloaderKind,
    active_slot: UpdateSlot,
    active_version: &str,
) -> crate::bootloader::BootloaderConfig {
    use crate::bootloader::{BootloaderConfig, SlotBootEntry};
    let mk_entry = |slot: UpdateSlot, version: &str| {
        let tag = match slot {
            UpdateSlot::A => 'a',
            UpdateSlot::B => 'b',
        };
        SlotBootEntry {
            slot,
            version: version.to_string(),
            linux: PathBuf::from(format!("/boot/slot-{tag}/vmlinuz")),
            initrd: PathBuf::from(format!("/boot/slot-{tag}/initrd.img")),
            cmdline: format!("root=UUID=slot-{tag} ro slot={slot:?}"),
        }
    };
    BootloaderConfig {
        kind,
        slot_a: mk_entry(
            UpdateSlot::A,
            if active_slot == UpdateSlot::A {
                active_version
            } else {
                ""
            },
        ),
        slot_b: mk_entry(
            UpdateSlot::B,
            if active_slot == UpdateSlot::B {
                active_version
            } else {
                ""
            },
        ),
        default: active_slot,
        next_default: None,
        boot_root: PathBuf::from("/boot"),
    }
}

impl UpdateEngine for AbUpdateEngine {
    async fn check_updates(&self) -> Result<Vec<UpdateManifest>, crate::UpdateError> {
        // 真实实现：GET {update_source}/manifests.json → 反序列化清单列表
        let url = format!(
            "{}/manifests.json",
            self.update_source.trim_end_matches('/')
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| crate::UpdateError::DownloadFailed(format!("拉取清单失败: {e}")))?;
        if !resp.status().is_success() {
            return Err(crate::UpdateError::DownloadFailed(format!(
                "清单源返回 HTTP {}",
                resp.status()
            )));
        }
        let manifests: Vec<UpdateManifest> = resp
            .json()
            .await
            .map_err(|e| crate::UpdateError::DownloadFailed(format!("清单解析失败: {e}")))?;
        if manifests.is_empty() {
            return Err(crate::UpdateError::NoUpdates);
        }
        Ok(manifests)
    }

    async fn download(&self, manifest: &UpdateManifest) -> Result<TaskId, crate::UpdateError> {
        // 真实下载：GET {update_source}/{version}.pkg → 暂存盘 → 推进 Completed。
        let task = TaskId::new();
        self.tasks
            .lock()
            .expect("tasks poisoned")
            .insert(task, UpdateStatus::Downloading { progress: 0.0 });

        // 确保 staging 目录存在
        std::fs::create_dir_all(&self.staging_dir)
            .map_err(|e| crate::UpdateError::DownloadFailed(format!("创建暂存目录失败: {e}")))?;
        let url = format!(
            "{}/{}.pkg",
            self.update_source.trim_end_matches('/'),
            manifest.version
        );
        let dest = self.staging_dir.join(format!("{}.pkg", manifest.version));
        let n = download_to_file(&self.client, &url, &dest).await?;
        let _ = n; // 字节数仅用于日志，暂不持久化
        self.tasks
            .lock()
            .expect("tasks poisoned")
            .insert(task, UpdateStatus::Completed);
        Ok(task)
    }

    async fn verify(
        &self,
        manifest: &UpdateManifest,
        downloaded_path: &Path,
    ) -> Result<bool, crate::UpdateError> {
        // 安全红线：必须真实验签（ed25519）+ sha256 比对，绝不绕过。
        // 复用 real::verify_package（fail-closed，任一失败即 VerificationFailed）。
        verify_package(manifest, downloaded_path, &self.pubkey)
    }

    async fn write_to_inactive_slot(
        &self,
        manifest: &UpdateManifest,
    ) -> Result<UpdateSlot, crate::UpdateError> {
        // 决策路径（纯逻辑，已可用）：选可写槽 → 标记 Updating
        let mut slot = self.slot.lock().expect("slot poisoned");
        let target = slot.writable_slot()?;
        slot.begin_write(target)?;
        // 真实 I/O：写入 bootloader 槽（待 ostree 依赖注册）；此处仅推进状态机，
        // 同步刷新 bootloader 配置中该槽 entry 的 version（供 menuentry 标题）。
        slot.finish_write(target, &manifest.version, chrono::Utc::now())?;
        drop(slot);
        // 同步 bootloader 配置 entry 版本
        {
            let mut bl = self.bootloader.lock().expect("bootloader poisoned");
            let entry = match target {
                UpdateSlot::A => &mut bl.slot_a,
                UpdateSlot::B => &mut bl.slot_b,
            };
            entry.version = manifest.version.clone();
        }
        Ok(target)
    }

    async fn activate_slot(&self, slot: UpdateSlot) -> Result<(), crate::UpdateError> {
        // 决策路径（纯逻辑）：规划激活 + 应用状态切换（在内存态先推进，
        // bootloader 失败时调用方应配合 on_boot_failed 回滚内存态）
        // —— 同步阶段：所有 Mutex 在 await 前释放（避免 holding lock across await）。
        let (target, plan, files) = {
            let mut sm = self.slot.lock().expect("slot poisoned");
            let decision = sm.plan_activation(slot);
            match decision {
                crate::slot::SlotSwitchDecision::Activate { target, previous } => {
                    sm.apply_activation(target, previous, chrono::Utc::now())?;
                    let (kind, files) = {
                        let mut bl = self.bootloader.lock().expect("bootloader poisoned");
                        // 先设 next_default（一次性，仅下次启动用 target），持久 default 不变——
                        // 这样即使 target 启动失败，下下次启动自动回到持久 default（boot_once fallback）。
                        bl.set_next_boot(target);
                        (bl.kind, bl.render())
                    };
                    let plan = crate::bootloader::ActivationPlan::new(kind, target);
                    (target, plan, files)
                }
                // 首启或已激活，无需切换
                _ => return Ok(()),
            }
        };
        // —— 异步阶段：所有锁已释放，可安全 await ——
        // 1. 写 bootloader 配置文件（原子写；生产路径用，fixture 测不触盘）
        crate::bootloader::write_config_files(&files)?;
        // 2. 调 bootloader 工具设 next-boot 一次性目标
        //    （grub2-reboot <id> / bootctl set-oneshot <id>）
        crate::bootloader::run_next_boot(self.runner.as_ref(), &plan).await?;
        // 注：commit 阶段（升级为持久 default）应在首启探活通过后调用
        // （run_commit + bootloader.set_default）；此处不自动 commit，
        // 由上层（OTA orchestrator）在收到 watchdog/health 信号后显式提交。
        // 这样保证坏槽永远不会变成持久 default（boot_once fallback 不变量）。
        let _ = target; // 已在 plan 内消费；此处仅保留供诊断
        Ok(())
    }

    async fn status(&self, task: &TaskId) -> UpdateStatus {
        let tasks = self.tasks.lock().expect("tasks poisoned");
        tasks.get(task).cloned().unwrap_or(UpdateStatus::Failed {
            reason: format!("任务不存在: {task}"),
        })
    }
}

// ============================================================================
// AbRollbackManager —— watchdog 自动回滚
// ============================================================================

/// A/B 双槽回滚管理器（默认实现）。
///
/// 持有 [`SlotManager`]（与 [`AbUpdateEngine`] 共享同状态机语义）+ 回滚策略 +
/// 连续失败计数。`verify_current_health` 待真实探针（systemd / RPC）依赖就绪，
/// `auto_rollback_if_unhealthy` 复用 [`should_rollback`] 纯决策。
pub struct AbRollbackManager {
    slot: Mutex<SlotManager>,
    policy: RollbackPolicy,
    consecutive_failures: Mutex<u32>,
}

impl AbRollbackManager {
    /// 构造：给定当前活动槽 + 版本 + 策略。
    #[must_use]
    pub fn new(
        active_slot: UpdateSlot,
        version: impl Into<String>,
        policy: RollbackPolicy,
    ) -> Self {
        Self {
            slot: Mutex::new(SlotManager::new(active_slot, version, chrono::Utc::now())),
            policy,
            consecutive_failures: Mutex::new(0),
        }
    }

    /// 取当前策略。
    #[must_use]
    pub fn policy(&self) -> RollbackPolicy {
        self.policy
    }
}

impl RollbackManager for AbRollbackManager {
    async fn list_snapshots(&self) -> Vec<RollbackPoint> {
        let sm = self.slot.lock().expect("slot poisoned");
        let mut points = Vec::new();
        for s in [sm.slot(UpdateSlot::A), sm.slot(UpdateSlot::B)] {
            if let Some(ver) = &s.version {
                if s.status != SlotStatus::Failed {
                    points.push(RollbackPoint {
                        slot: s.slot,
                        version: ver.clone(),
                        created_at: s.last_activated_at.unwrap_or_else(chrono::Utc::now),
                        healthy: s.status == SlotStatus::Active,
                    });
                }
            }
        }
        points
    }

    async fn rollback_to(&self, point: &RollbackPoint) -> Result<(), crate::UpdateError> {
        // 决策路径：触发 on_boot_failed 式回滚（标记当前 active 为 failed，恢复目标）
        let mut sm = self.slot.lock().expect("slot poisoned");
        // 直接设置目标槽为 Active，当前 active 降级
        if let Some(current) = sm.active_slot() {
            if current != point.slot {
                sm.slot_mut(current).status = SlotStatus::Failed;
            }
        }
        let t = sm.slot_mut(point.slot);
        t.status = SlotStatus::Active;
        // 真实 I/O：bootloader 切回旧槽（待依赖注册）
        // 此处仅推进内存态，调用方应配合 bootloader 实际切换
        Ok(())
    }

    async fn verify_current_health(&self) -> Result<HealthReport, crate::UpdateError> {
        // 真实实现：探针（RPC / systemd is-system-running / 自定义健康端点）
        todo!("verify_current_health：探针实现，待健康检查依赖注册")
    }

    async fn auto_rollback_if_unhealthy(&self) -> Result<bool, crate::UpdateError> {
        // 决策路径（纯逻辑）：用 should_rollback 判定。
        // 健康状态由 verify_current_health 提供（此处占位 Healthy，待真实探针接入）。
        // 注：真实接入后应先调 verify_current_health 取 HealthReport，再判定。
        let sm = self.slot.lock().expect("slot poisoned");
        let has_target = sm.previous_active_slot().is_some();
        let failures = *self.consecutive_failures.lock().expect("failures poisoned");
        let ctx = RollbackContext::new(
            os_core::Health::Healthy, // 占位：真实接入由探针决定
            self.policy,
            failures,
            has_target,
        );
        match should_rollback(&ctx) {
            RollbackDecision::RollbackNow { .. } => {
                // 推进状态机回滚
                let mut sm = self.slot.lock().expect("slot poisoned");
                let decision = sm.on_boot_failed();
                Ok(matches!(
                    decision,
                    crate::slot::SlotSwitchDecision::Rollback { .. }
                ))
            }
            _ => Ok(false),
        }
    }
}

// ============================================================================
// NvdCveMonitor —— CVE 监听
// ============================================================================

/// NVD/OSV CVE 监听器（默认实现）。
///
/// `check_advisories` 走真实 reqwest：POST OSV `/query` 批量接口（每个 watched
/// component 一条 package 查询），汇总响应后用 [`parse_osv_advisories`] 过滤解析。
/// `subscribe` 链式注册回调（纯内存，已可用）。
///
/// OSV API 选型：NVD 官方 API 有匿名配额限制 + key 申请门槛；OSV 聚合 NVD/PyPI/
/// RustSec/GHSA 等多源，schema 统一、匿名可用，更适合开源 OS 工作流。
pub struct NvdCveMonitor {
    callbacks: Mutex<Vec<Box<dyn CveCallback>>>,
    /// 受监控的组件清单（如 samba/qemu/rdma-core）
    watched_components: Vec<String>,
    /// reqwest 客户端
    client: reqwest::Client,
    /// OSV API 根 URL（默认 <https://api.osv.dev/v1>）
    api_url: String,
}

impl NvdCveMonitor {
    /// 构造：给定受监控组件清单。默认对接 OSV 官方 API。
    #[must_use]
    pub fn new(watched_components: Vec<String>) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("os-update/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("reqwest client 构造失败");
        Self {
            callbacks: Mutex::new(Vec::new()),
            watched_components,
            client,
            api_url: "https://api.osv.dev/v1".to_string(),
        }
    }

    /// 覆盖 OSV API URL（用于 fixture 测试或私有镜像）。
    #[must_use]
    pub fn with_api_url(mut self, url: impl Into<String>) -> Self {
        self.api_url = url.into();
        self
    }

    /// 注入自定义 reqwest 客户端（用于 fixture 测试改写 base_url）。
    #[must_use]
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// 受监控组件清单（只读）。
    pub fn watched_components(&self) -> &[String] {
        &self.watched_components
    }
}

impl CveMonitor for NvdCveMonitor {
    async fn check_advisories(&self) -> Result<Vec<CveAdvisory>, crate::UpdateError> {
        // 真实实现：逐组件 POST OSV /query，合并响应体（多 JSON 对象拼接为
        // 单个 {"vulns":[...]} 数组供解析器统一处理）。
        let mut all_vulns: Vec<serde_json::Value> = Vec::new();
        for component in &self.watched_components {
            // OSV /query：仅按 package.name 查询（不绑定 ecosystem，跨源聚合命中更广；
            // 调用方可对结果按 component 名再过滤，已在 parse_osv_advisories 完成）
            let req = serde_json::json!({
                "package": {"name": component}
            });
            let url = format!("{}/query", self.api_url.trim_end_matches('/'));
            let resp = self
                .client
                .post(&url)
                .json(&req)
                .send()
                .await
                .map_err(|e| {
                    crate::UpdateError::CveCheckFailed(format!("OSV 查询 {component} 失败: {e}"))
                })?;
            if !resp.status().is_success() {
                return Err(crate::UpdateError::CveCheckFailed(format!(
                    "OSV 查询 {component} 返回 HTTP {}",
                    resp.status()
                )));
            }
            let v: serde_json::Value = resp.json().await.map_err(|e| {
                crate::UpdateError::CveCheckFailed(format!("OSV 响应解析失败: {e}"))
            })?;
            if let Some(arr) = v.get("vulns").and_then(|x| x.as_array()) {
                all_vulns.extend(arr.iter().cloned());
            }
        }
        let merged = serde_json::to_string(&serde_json::json!({ "vulns": all_vulns }))
            .map_err(|e| crate::UpdateError::CveCheckFailed(format!("合并响应失败: {e}")))?;
        parse_osv_advisories(&merged, &self.watched_components)
    }

    async fn subscribe(&self, callback: Box<dyn CveCallback>) {
        self.callbacks
            .lock()
            .expect("callbacks poisoned")
            .push(callback);
    }
}

// ============================================================================
// HaRollingUpgrade —— HA 集群滚动升级
// ============================================================================

/// HA 滚动升级编排器（默认实现）。
///
/// `plan` 复用 [`decide_upgrade_order`] 纯决策（已可用）；`execute` 的真实逐节点
/// 升级编排待 meta-agent（leader 选举）+ bootloader 依赖注册后填充。
pub struct HaRollingUpgrade {
    /// 当前集群成员快照（由 Consensus::get_members 提供）
    members: Mutex<Vec<NodeInfo>>,
    /// 任务状态表（TaskId → RollingStatus）
    tasks: Mutex<HashMap<TaskId, RollingStateMachine>>,
}

impl HaRollingUpgrade {
    /// 构造：给定初始集群成员快照。
    #[must_use]
    pub fn new(members: Vec<NodeInfo>) -> Self {
        Self {
            members: Mutex::new(members),
            tasks: Mutex::new(HashMap::new()),
        }
    }

    /// 更新集群成员快照（leader 变更后刷新）。
    pub fn set_members(&self, members: Vec<NodeInfo>) {
        *self.members.lock().expect("members poisoned") = members;
    }
}

impl RollingUpgrade for HaRollingUpgrade {
    async fn plan(
        &self,
        _manifest: &UpdateManifest,
        strategy: RollingStrategy,
    ) -> Result<RollingPlan, crate::UpdateError> {
        // 决策路径（纯逻辑）：按策略排定节点顺序
        let members = self.members.lock().expect("members poisoned").clone();
        let order = decide_upgrade_order(&members, strategy)?;
        Ok(RollingPlan {
            order,
            strategy,
            per_node_verify: true,
        })
    }

    async fn execute(&self, plan: RollingPlan) -> Result<TaskId, crate::UpdateError> {
        // 真实实现：逐节点串行升级（follower 先 → 验证 → leader），每节点调
        // UpdateEngine 完成单节点 OTA + 健康验证。待 meta/bootloader 依赖注册。
        let task = TaskId::new();
        let sm = RollingStateMachine::new(plan);
        self.tasks.lock().expect("tasks poisoned").insert(task, sm);
        todo!("execute：逐节点升级编排，待 meta leader 选举 + bootloader 依赖注册")
    }

    async fn status(&self, task: &TaskId) -> RollingStatus {
        let tasks = self.tasks.lock().expect("tasks poisoned");
        tasks
            .get(task)
            .map(|sm| sm.state.clone())
            .unwrap_or(RollingStatus::Failed {
                failed_node: NodeId::new("unknown"),
                reason: format!("任务不存在: {task}"),
            })
    }
}

// ----------------------------------------------------------------------------
// 单元测试（真实 reqwest 下载/验签 + CVE 轮询，用本地 TcpListener fixture）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::real::sha256_hex_bytes;
    use crate::update::ComponentUpdate;
    use crate::CveSeverity;
    use base64::Engine;
    use ed25519_dalek::{Signer, SigningKey};
    use sha2::{Digest, Sha256};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// 极简 HTTP/1.0 fixture 服务器：按 path 返回固定 body。
    ///
    /// 启动在 127.0.0.1 随机端口，单连接处理（足够测试往返）。返回
    /// `(base_url, join_handle)`；handle 内循环读到 EOF 后退出。
    async fn start_fixture(
        routes: std::sync::Arc<std::collections::HashMap<String, Vec<u8>>>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let handle = tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(p) => p,
                    Err(_) => break,
                };
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    // 解析请求行：GET /path HTTP/1.1
                    let path = req
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    // POST 也用同一 path（OSV /query），body 不解析
                    let body = routes.get(&path).cloned();
                    let resp = match body {
                        Some(b) => {
                            let head = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                b.len()
                            );
                            let mut out = head.into_bytes();
                            out.extend_from_slice(&b);
                            out
                        }
                        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                    };
                    let _ = sock.write_all(&resp).await;
                    let _ = sock.shutdown().await;
                    drop(routes);
                });
            }
        });
        (url, handle)
    }

    fn manifest_for(payload: &[u8], signing: &SigningKey) -> UpdateManifest {
        let sha = sha256_hex_bytes(payload);
        let mut h = Sha256::new();
        h.update(payload);
        let digest = h.finalize();
        let sig = signing.sign(&digest);
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
        UpdateManifest {
            version: "2.0.0".to_string(),
            release_notes: String::new(),
            size_bytes: payload.len() as u64,
            sha256: sha,
            signature: sig_b64,
            min_current_version: None,
            components: vec![ComponentUpdate {
                name: "osd".to_string(),
                version: "2.0.0".to_string(),
                restart_required: true,
            }],
        }
    }

    #[tokio::test]
    async fn download_writes_file_and_verify_passes() {
        // 准备 fixture：/2.0.0.pkg 返回真实 payload
        let signing = SigningKey::from_bytes(&[42u8; 32]);
        let payload = b"real update payload for roundtrip test";
        let manifest = manifest_for(payload, &signing);

        let mut routes = std::collections::HashMap::new();
        routes.insert("/2.0.0.pkg".to_string(), payload.to_vec());
        let (base, _h) = start_fixture(std::sync::Arc::new(routes)).await;

        let staging = tempfile::tempdir().unwrap();
        let engine = AbUpdateEngine::new(
            UpdateSlot::A,
            "1.0.0",
            &base,
            signing.verifying_key().to_bytes(),
            crate::bootloader::BootloaderKind::Grub,
        )
        .with_staging_dir(staging.path());

        // 下载
        let task = engine.download(&manifest).await.unwrap();
        assert!(matches!(
            engine.status(&task).await,
            UpdateStatus::Completed
        ));
        let pkg = staging.path().join("2.0.0.pkg");
        assert!(pkg.exists());
        assert_eq!(std::fs::read(&pkg).unwrap(), payload);

        // 验签（真实 ed25519 + sha256）
        let ok = engine.verify(&manifest, &pkg).await.unwrap();
        assert!(ok);
    }

    #[tokio::test]
    async fn download_404_returns_download_failed() {
        let (base, _h) = start_fixture(std::sync::Arc::new(std::collections::HashMap::new())).await;
        let signing = SigningKey::from_bytes(&[1u8; 32]);
        let staging = tempfile::tempdir().unwrap();
        let engine = AbUpdateEngine::new(
            UpdateSlot::A,
            "1.0.0",
            &base,
            signing.verifying_key().to_bytes(),
            crate::bootloader::BootloaderKind::Grub,
        )
        .with_staging_dir(staging.path());
        let manifest = manifest_for(b"x", &signing);
        let err = engine.download(&manifest).await.unwrap_err();
        assert!(matches!(err, crate::UpdateError::DownloadFailed(_)));
    }

    #[tokio::test]
    async fn verify_rejects_tampered_download() {
        let signing = SigningKey::from_bytes(&[9u8; 32]);
        let payload = b"original payload";
        let manifest = manifest_for(payload, &signing);
        let staging = tempfile::tempdir().unwrap();
        let engine = AbUpdateEngine::new(
            UpdateSlot::A,
            "1.0.0",
            "http://unused.example",
            signing.verifying_key().to_bytes(),
            crate::bootloader::BootloaderKind::Grub,
        )
        .with_staging_dir(staging.path());
        // 写入篡改后的"下载"
        let pkg = staging.path().join("2.0.0.pkg");
        std::fs::write(&pkg, b"tampered payload!!!").unwrap();
        let err = engine.verify(&manifest, &pkg).await.unwrap_err();
        assert!(matches!(err, crate::UpdateError::VerificationFailed(_)));
    }

    #[tokio::test]
    async fn check_updates_parses_manifest_list() {
        let signing = SigningKey::from_bytes(&[5u8; 32]);
        let m = manifest_for(b"pkg", &signing);
        let body = serde_json::to_vec(&vec![m.clone()]).unwrap();
        let mut routes = std::collections::HashMap::new();
        routes.insert("/manifests.json".to_string(), body);
        let (base, _h) = start_fixture(std::sync::Arc::new(routes)).await;
        let engine = AbUpdateEngine::new(
            UpdateSlot::A,
            "1.0.0",
            &base,
            signing.verifying_key().to_bytes(),
            crate::bootloader::BootloaderKind::Grub,
        );
        let list = engine.check_updates().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].version, "2.0.0");
    }

    #[tokio::test]
    async fn check_updates_empty_returns_no_updates() {
        let signing = SigningKey::from_bytes(&[6u8; 32]);
        let mut routes = std::collections::HashMap::new();
        routes.insert(
            "/manifests.json".to_string(),
            serde_json::to_vec(&Vec::<UpdateManifest>::new()).unwrap(),
        );
        let (base, _h) = start_fixture(std::sync::Arc::new(routes)).await;
        let engine = AbUpdateEngine::new(
            UpdateSlot::A,
            "1.0.0",
            &base,
            signing.verifying_key().to_bytes(),
            crate::bootloader::BootloaderKind::Grub,
        );
        assert!(matches!(
            engine.check_updates().await,
            Err(crate::UpdateError::NoUpdates)
        ));
    }

    #[tokio::test]
    async fn cve_monitor_polls_osv_and_filters() {
        // 模拟 OSV /query 响应：含一条 samba 漏洞
        let osv_body = br#"{
            "vulns": [{
                "id": "OSV-2024-Z",
                "aliases": ["CVE-2024-5555"],
                "summary": "Critical RCE in samba daemon",
                "affected": [{"package": {"name": "samba"}, "ranges": [{"events": [{"type":"fixed","value":"4.21"}]}]}],
                "published": "2024-05-01T00:00:00Z"
            }]
        }"#;
        let mut routes = std::collections::HashMap::new();
        routes.insert("/query".to_string(), osv_body.to_vec());
        let (base, _h) = start_fixture(std::sync::Arc::new(routes)).await;
        let monitor = NvdCveMonitor::new(vec!["samba".to_string()]).with_api_url(&base);
        let list = monitor.check_advisories().await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].cve_id, "CVE-2024-5555");
        assert_eq!(list[0].affected_component, "samba");
        assert_eq!(list[0].fixed_version, "4.21");
        assert_eq!(list[0].severity, CveSeverity::Critical);
    }

    #[tokio::test]
    async fn cve_monitor_empty_response() {
        let mut routes = std::collections::HashMap::new();
        routes.insert("/query".to_string(), b"{\"vulns\":[]}".to_vec());
        let (base, _h) = start_fixture(std::sync::Arc::new(routes)).await;
        let monitor = NvdCveMonitor::new(vec!["samba".to_string()]).with_api_url(&base);
        let list = monitor.check_advisories().await.unwrap();
        assert!(list.is_empty());
    }

    #[tokio::test]
    async fn cve_monitor_upstream_500_fails() {
        // 用一个返回 500 的 fixture：直接构造 500 响应
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}");
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let resp = b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            let _ = sock.write_all(resp).await;
            let _ = sock.shutdown().await;
        });
        let monitor = NvdCveMonitor::new(vec!["samba".to_string()]).with_api_url(&url);
        let err = monitor.check_advisories().await.unwrap_err();
        assert!(matches!(err, crate::UpdateError::CveCheckFailed(_)));
        let _ = handle.await;
    }

    #[tokio::test]
    async fn cve_monitor_subscribe_collects_callbacks() {
        struct Noop;
        #[async_trait::async_trait]
        impl CveCallback for Noop {
            async fn on_advisory(&self, _: &CveAdvisory) {}
        }
        let monitor = NvdCveMonitor::new(vec![]);
        monitor.subscribe(Box::new(Noop)).await;
        monitor.subscribe(Box::new(Noop)).await;
        assert_eq!(monitor.callbacks.lock().unwrap().len(), 2);
    }

    // —— activate_slot 真实 bootloader 编排（fixture runner）——

    /// 单条调用记录（program + args）。
    type Call = (String, Vec<String>);
    /// 单条预设输出（program + args 首元素 + 输出）。
    type Fixture = (String, String, crate::bootloader::BootloaderCommandOutput);

    /// 测试用 bootloader fixture runner：记录所有调用 + 按 (program, args 首元素)
    /// 分发预设输出；未注册的默认返回 ok 空输出（成功）。
    /// 用 `Arc<Mutex<...>>` 内部态，可 clone（克隆后共享记录）。
    #[derive(Clone)]
    struct RecordingRunner {
        calls: std::sync::Arc<std::sync::Mutex<Vec<Call>>>,
        outputs: std::sync::Arc<std::sync::Mutex<Vec<Fixture>>>,
    }

    impl RecordingRunner {
        fn new() -> Self {
            Self {
                calls: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                outputs: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }

        fn on(
            self,
            program: &str,
            args_first: &str,
            output: crate::bootloader::BootloaderCommandOutput,
        ) -> Self {
            self.outputs.lock().unwrap().push((
                program.to_string(),
                args_first.to_string(),
                output,
            ));
            self
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl crate::bootloader::BootloaderRunner for RecordingRunner {
        async fn run(
            &self,
            program: &str,
            args: &[String],
        ) -> Result<crate::bootloader::BootloaderCommandOutput, crate::UpdateError> {
            self.calls
                .lock()
                .unwrap()
                .push((program.to_string(), args.to_vec()));
            let first = args.first().map(String::as_str).unwrap_or("");
            let outputs = self.outputs.lock().unwrap();
            for (p, a, o) in outputs.iter() {
                if p == program && (a == first || a.is_empty()) {
                    return Ok(o.clone());
                }
            }
            Ok(crate::bootloader::BootloaderCommandOutput::ok())
        }
    }

    /// 测试用 bootloader 配置：路径指向临时目录（避免 fixture 测试触盘写 /boot）。
    fn test_bootloader_config(
        kind: crate::bootloader::BootloaderKind,
        active: UpdateSlot,
        boot_root: &Path,
    ) -> crate::bootloader::BootloaderConfig {
        use crate::bootloader::{BootloaderConfig, SlotBootEntry};
        let mk = |slot: UpdateSlot, ver: &str| {
            let tag = match slot {
                UpdateSlot::A => 'a',
                UpdateSlot::B => 'b',
            };
            SlotBootEntry {
                slot,
                version: ver.to_string(),
                linux: boot_root.join(format!("slot-{tag}/vmlinuz")),
                initrd: boot_root.join(format!("slot-{tag}/initrd.img")),
                cmdline: format!("root=UUID=slot-{tag} ro slot={slot:?}"),
            }
        };
        BootloaderConfig {
            kind,
            slot_a: mk(
                UpdateSlot::A,
                if active == UpdateSlot::A { "1.0.0" } else { "" },
            ),
            slot_b: mk(
                UpdateSlot::B,
                if active == UpdateSlot::B { "1.0.0" } else { "" },
            ),
            default: active,
            next_default: None,
            boot_root: boot_root.to_path_buf(),
        }
    }

    #[tokio::test]
    async fn activate_slot_grub_runs_grub2_reboot_and_writes_config() {
        let signing = SigningKey::from_bytes(&[11u8; 32]);
        let staging = tempfile::tempdir().unwrap();
        let boot_root = tempfile::tempdir().unwrap();
        let runner = RecordingRunner::new();
        let engine = AbUpdateEngine::new(
            UpdateSlot::A,
            "1.0.0",
            "http://unused.example",
            signing.verifying_key().to_bytes(),
            crate::bootloader::BootloaderKind::Grub,
        )
        .with_staging_dir(staging.path())
        .with_runner(Box::new(runner))
        .with_bootloader(test_bootloader_config(
            crate::bootloader::BootloaderKind::Grub,
            UpdateSlot::A,
            boot_root.path(),
        ));

        // 先写入 B 槽（让 plan_activation 给出 Activate 决策）
        let m = manifest_for(b"payload", &signing);
        let written = engine.write_to_inactive_slot(&m).await.unwrap();
        assert_eq!(written, UpdateSlot::B);
        // 激活 B
        engine.activate_slot(UpdateSlot::B).await.unwrap();

        // bootloader 配置应已渲染写盘（GRUB → 单个 grub.cfg）
        let grub_cfg = boot_root.path().join("grub/grub.cfg");
        assert!(grub_cfg.exists(), "grub.cfg 应已写盘");
        let content = std::fs::read_to_string(&grub_cfg).unwrap();
        assert!(content.contains("set default=os_slot_a"));
        assert!(content.contains("next-boot oneshot"));
        assert!(content.contains("os_slot_b"));

        // 状态机：B 激活、A 降级
        let sm = engine.slot_manager();
        assert_eq!(sm.active_slot(), Some(UpdateSlot::B));

        // bootloader 配置：default 仍为 A（未 commit），next_default=B（一次性）
        let bl = engine.bootloader_config();
        assert_eq!(bl.default, UpdateSlot::A);
        assert_eq!(bl.next_default, Some(UpdateSlot::B));
    }

    #[tokio::test]
    async fn activate_slot_systemd_boot_writes_entry_files() {
        let signing = SigningKey::from_bytes(&[12u8; 32]);
        let staging = tempfile::tempdir().unwrap();
        let boot_root = tempfile::tempdir().unwrap();
        let runner = RecordingRunner::new();
        let engine = AbUpdateEngine::new(
            UpdateSlot::A,
            "1.0.0",
            "http://unused.example",
            signing.verifying_key().to_bytes(),
            crate::bootloader::BootloaderKind::SystemdBoot,
        )
        .with_staging_dir(staging.path())
        .with_runner(Box::new(runner))
        .with_bootloader(test_bootloader_config(
            crate::bootloader::BootloaderKind::SystemdBoot,
            UpdateSlot::A,
            boot_root.path(),
        ));

        let m = manifest_for(b"payload", &signing);
        engine.write_to_inactive_slot(&m).await.unwrap();
        engine.activate_slot(UpdateSlot::B).await.unwrap();

        // systemd-boot → 3 个文件
        let entries_a = boot_root.path().join("loader/entries/os-slot-a.conf");
        let entries_b = boot_root.path().join("loader/entries/os-slot-b.conf");
        let loader_conf = boot_root.path().join("loader/loader.conf");
        assert!(entries_a.exists());
        assert!(entries_b.exists());
        assert!(loader_conf.exists());
        let lc = std::fs::read_to_string(&loader_conf).unwrap();
        assert!(lc.contains("default os-slot-a"));
        assert!(lc.contains("next-boot oneshot"));
    }

    #[tokio::test]
    async fn activate_slot_noop_when_already_active() {
        let signing = SigningKey::from_bytes(&[13u8; 32]);
        let staging = tempfile::tempdir().unwrap();
        let boot_root = tempfile::tempdir().unwrap();
        let runner = RecordingRunner::new();
        let engine = AbUpdateEngine::new(
            UpdateSlot::A,
            "1.0.0",
            "http://unused.example",
            signing.verifying_key().to_bytes(),
            crate::bootloader::BootloaderKind::Grub,
        )
        .with_staging_dir(staging.path())
        .with_runner(Box::new(runner.clone()))
        .with_bootloader(test_bootloader_config(
            crate::bootloader::BootloaderKind::Grub,
            UpdateSlot::A,
            boot_root.path(),
        ));
        // 激活当前已 active 的 A 槽 → plan_activation 返回 NoOp（A 非 Inactive），
        // 不调用 bootloader 工具
        engine.activate_slot(UpdateSlot::A).await.unwrap();
        assert!(runner.calls().is_empty(), "NoOp 路径不应调 bootloader 工具");
    }

    #[tokio::test]
    async fn activate_slot_runner_failure_returns_slot_conflict() {
        let signing = SigningKey::from_bytes(&[14u8; 32]);
        let staging = tempfile::tempdir().unwrap();
        let boot_root = tempfile::tempdir().unwrap();
        let runner = RecordingRunner::new().on(
            "grub2-reboot",
            "os_slot_b",
            crate::bootloader::BootloaderCommandOutput {
                status: 1,
                stdout: String::new(),
                stderr: "permission denied (need root)".to_string(),
            },
        );
        let engine = AbUpdateEngine::new(
            UpdateSlot::A,
            "1.0.0",
            "http://unused.example",
            signing.verifying_key().to_bytes(),
            crate::bootloader::BootloaderKind::Grub,
        )
        .with_staging_dir(staging.path())
        .with_runner(Box::new(runner))
        .with_bootloader(test_bootloader_config(
            crate::bootloader::BootloaderKind::Grub,
            UpdateSlot::A,
            boot_root.path(),
        ));
        let m = manifest_for(b"payload", &signing);
        engine.write_to_inactive_slot(&m).await.unwrap();
        let err = engine.activate_slot(UpdateSlot::B).await.unwrap_err();
        assert!(matches!(err, crate::UpdateError::SlotConflict(_)));
    }
}
