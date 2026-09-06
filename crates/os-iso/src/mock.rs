//! `MockIsoBuilder` / `MockInstaller` —— 纯内存实现，供下游测试注入。
//!
//! 仅在 `mock` feature 下编译。下游（update-agent / api-agent）在 `[dev-dependencies]`
//! 加 `os-iso = { workspace = true, features = ["mock"] }`。
//!
//! 设计（见 `_conventions.md §5`）：
//! - 实现完整 [`IsoBuilder`] / [`Installer`] trait，**不依赖外部状态**（无 xorriso /
//!   无 squashfs / 无裸机）。
//! - 提供构造器预置返回值与状态推进。
//! - `MockIsoBuilder::build` 注册任务并立即推进到 `Completed`（确定性产物）；
//!   `status` 返回对应状态。`verify` 返回构造器预置结果（默认 true）。
//! - `MockInstaller::detect_hardware` 返回构造器预置报告；
//!   `install` 返回构造器预置报告（或注入错误）。
//! - 错误注入：`with_build_error` / `with_install_error` 让下游测错误路径。

#![cfg(feature = "mock")]

use crate::installer::{HardwareReport, InstallReport, InstallTarget, Installer};
use crate::iso::{IsoBuildResult, IsoBuildStatus, IsoBuilder, IsoSpec, IsoVariant};
use crate::IsoError;
use os_core::TaskId;
use std::collections::HashMap;
use std::sync::Mutex;

// ----------------------------------------------------------------------------
// MockIsoBuilder
// ----------------------------------------------------------------------------

/// Mock ISO 构建器——纯内存、确定性。
///
/// 内部任务表：`TaskId` → `IsoBuildStatus`。`build` 注册任务并立即标记 `Completed`
/// （附确定性产物）。下游可：
/// - `with_initial_status` 预置某个已存在任务的状态（测 `status` 查询分支）。
/// - `with_build_error` 让后续 `build` 返回错误（测错误路径）。
/// - `with_verify_result` 让 `verify` 返回指定结果。
pub struct MockIsoBuilder {
    tasks: Mutex<HashMap<TaskId, IsoBuildStatus>>,
    /// 注入的构建错误（下次 build 抛出后清空）。
    build_error: Mutex<Option<IsoError>>,
    /// verify 默认返回值。
    verify_result: Mutex<bool>,
    /// 产物计数器（构造确定性 sha256/size）。
    counter: Mutex<u64>,
}

impl Default for MockIsoBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl MockIsoBuilder {
    /// 构造空 mock（无任务、verify 返回 true）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            build_error: Mutex::new(None),
            verify_result: Mutex::new(true),
            counter: Mutex::new(0),
        }
    }

    /// 预置一个已存在的任务及其状态（测 `status` 查询）。
    #[must_use]
    pub fn with_initial_status(self, task: TaskId, status: IsoBuildStatus) -> Self {
        {
            let mut g = self.tasks.lock().expect("mock poisoned");
            g.insert(task, status);
        }
        self
    }

    /// 注入构建错误（下次 `build` 抛出此错误后清空）。
    #[must_use]
    pub fn with_build_error(self, err: IsoError) -> Self {
        *self.build_error.lock().expect("mock poisoned") = Some(err);
        self
    }

    /// 设置 `verify` 的返回值。
    #[must_use]
    pub fn with_verify_result(self, ok: bool) -> Self {
        *self.verify_result.lock().expect("mock poisoned") = ok;
        self
    }

    /// 构造确定性产物（按计数器派生 sha256/size）。
    fn make_deterministic_result(&self, spec: &IsoSpec) -> IsoBuildResult {
        let mut cnt = self.counter.lock().expect("mock poisoned");
        *cnt += 1;
        let n = *cnt;
        // 派生路径（确定性，便于断言）
        let name = match &spec.variant {
            IsoVariant::Standard => "std",
            IsoVariant::Clone { .. } => "clone",
        };
        let iso_path = std::path::PathBuf::from(format!(
            "/tmp/mock-os-iso/{name}-{}-{}.iso",
            spec.ubuntu_version, n
        ));
        // sha256：64 位 hex（用计数器填，便于断言唯一性）
        let sha = format!("{:064x}", n);
        let size_bytes = 1024 * 1024 * 100 * n; // 100 MiB × n
        crate::impl_iso::XorrisoIsoBuilder::make_build_result(iso_path, sha, size_bytes)
    }
}

impl IsoBuilder for MockIsoBuilder {
    async fn build(&self, mut spec: IsoSpec) -> Result<TaskId, IsoError> {
        // 注入错误（优先）
        if let Some(err) = self.build_error.lock().expect("mock poisoned").take() {
            return Err(err);
        }
        // 与真实 builder 一致：校验 + 克隆清洗
        spec.validate()?;
        spec.sanitize_clone_snapshot();
        let task = TaskId::new();
        let result = self.make_deterministic_result(&spec);
        let mut g = self.tasks.lock().expect("mock poisoned");
        g.insert(task, IsoBuildStatus::Completed(result));
        Ok(task)
    }

    async fn status(&self, task: &TaskId) -> IsoBuildStatus {
        let g = self.tasks.lock().expect("mock poisoned");
        g.get(task)
            .cloned()
            .unwrap_or_else(|| IsoBuildStatus::Failed {
                reason: format!("任务不存在: {task}"),
            })
    }

    async fn verify(
        &self,
        _iso_path: &std::path::Path,
        _expected_sha256: &str,
    ) -> Result<bool, IsoError> {
        Ok(*self.verify_result.lock().expect("mock poisoned"))
    }
}

// ----------------------------------------------------------------------------
// MockInstaller
// ----------------------------------------------------------------------------

/// Mock 安装器——纯内存、确定性。
///
/// - `detect_hardware` 返回构造器预置的 `HardwareReport`（默认占位报告）。
/// - `install` 返回构造器预置的 `InstallReport`（默认占位报告），或注入错误。
pub struct MockInstaller {
    hardware: Mutex<Option<HardwareReport>>,
    install_report: Mutex<Option<InstallReport>>,
    install_error: Mutex<Option<IsoError>>,
    /// 是否跳过 target 校验（默认 false，即校验，便于下游测非法 target）。
    skip_validation: bool,
}

impl Default for MockInstaller {
    fn default() -> Self {
        Self::new()
    }
}

impl MockInstaller {
    /// 构造：默认占位硬件报告 + 默认安装报告。
    #[must_use]
    pub fn new() -> Self {
        let hw = HardwareReport {
            cpu: "Mock CPU".to_string(),
            memory_gb: 16,
            disks: vec![crate::impl_installer::RustInstaller::placeholder_disk(
                "/dev/sda", 500,
            )],
            nics: vec!["eth0".to_string()],
            kvm_support: true,
            warnings: Vec::new(),
        };
        let report = InstallReport {
            installed_components: vec![
                "osd".to_string(),
                "os-storage".to_string(),
                "os-meta".to_string(),
            ],
            pool_created: Some("tank".to_string()),
            duration_secs: 0,
            post_install_actions: vec![
                "首次登录强制重设 root 密码".to_string(),
                "初始化管理员用户: admin".to_string(),
            ],
            commands: Vec::new(),
        };
        Self {
            hardware: Mutex::new(Some(hw)),
            install_report: Mutex::new(Some(report)),
            install_error: Mutex::new(None),
            skip_validation: false,
        }
    }

    /// 预置 `detect_hardware` 返回值。
    #[must_use]
    pub fn with_hardware(self, hw: HardwareReport) -> Self {
        *self.hardware.lock().expect("mock poisoned") = Some(hw);
        self
    }

    /// 预置 `install` 返回值。
    #[must_use]
    pub fn with_install_report(self, report: InstallReport) -> Self {
        *self.install_report.lock().expect("mock poisoned") = Some(report);
        self
    }

    /// 注入安装错误（下次 `install` 抛出后清空）。
    #[must_use]
    pub fn with_install_error(self, err: IsoError) -> Self {
        *self.install_error.lock().expect("mock poisoned") = Some(err);
        self
    }

    /// 跳过 target 校验（让下游测不构造合法 target 的场景）。
    #[must_use]
    pub fn skip_target_validation(self) -> Self {
        self.set_skip_validation(true)
    }

    /// 显式设置是否跳过 target 校验。
    #[must_use]
    pub fn set_skip_validation(mut self, skip: bool) -> Self {
        self.skip_validation = skip;
        self
    }
}

impl Installer for MockInstaller {
    async fn detect_hardware(&self) -> Result<HardwareReport, IsoError> {
        let g = self.hardware.lock().expect("mock poisoned");
        Ok(g.clone().unwrap_or_else(|| HardwareReport {
            cpu: "unknown".to_string(),
            memory_gb: 0,
            disks: Vec::new(),
            nics: Vec::new(),
            kvm_support: false,
            warnings: Vec::new(),
        }))
    }

    async fn install(
        &self,
        _iso_path: &std::path::Path,
        target: InstallTarget,
    ) -> Result<InstallReport, IsoError> {
        // 注入错误优先
        if let Some(err) = self.install_error.lock().expect("mock poisoned").take() {
            return Err(err);
        }
        if !self.skip_validation {
            target.validate()?;
            target.validate_raid_disk_count()?;
        }
        // 取预置报告，并填充 post_install_actions 中的 admin_user（若调用方指定了）
        let mut report = self
            .install_report
            .lock()
            .expect("mock poisoned")
            .clone()
            .unwrap_or_else(|| InstallReport {
                installed_components: Vec::new(),
                pool_created: None,
                duration_secs: 0,
                post_install_actions: Vec::new(),
                commands: Vec::new(),
            });
        // 把 admin_user 注入首启动作（动态化）
        let admin = target.admin_user.clone();
        report.post_install_actions = report
            .post_install_actions
            .iter()
            .map(|a| {
                if a.starts_with("初始化管理员用户") {
                    format!("初始化管理员用户: {admin}")
                } else {
                    a.clone()
                }
            })
            .collect();
        Ok(report)
    }
}

/// dyn 兼容性说明（呼应 ADR-COMPAT-001）：
///
/// 本 crate 的 `IsoBuilder` / `Installer` 保持原生 `async fn in trait`（单实现为主，
/// 非 dyn 派发），因此**不能** `Box<dyn IsoBuilder>`。下游（update-agent / api-agent）
/// 应以**具体类型或泛型**注入 mock，例如：
/// ```ignore
/// struct UpdateService<B: IsoBuilder> { builder: B, ... }
/// // 测试：UpdateService { builder: MockIsoBuilder::new(), ... }
/// ```
/// 若下游确需 `Box<dyn>` 运行期多态，须走 ADR 给 trait 加 `#[async_trait]`
/// （会签本 agent + 下游 agent）。当前不预设。
#[doc(hidden)]
pub fn _dyn_compat_note() {}

// ----------------------------------------------------------------------------
// 单元测试（仅 mock feature 下编译）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iso::{IsoSpec, IsoVariant};
    use serde_json::json;
    use std::path::Path;

    fn std_spec() -> IsoSpec {
        IsoSpec {
            variant: IsoVariant::Standard,
            base_image: "ubuntu-24.04-base.squashfs".to_string(),
            components: vec!["osd".to_string()],
            ubuntu_version: "24.04".to_string(),
            arch: "x86_64".to_string(),
            locale: "zh_CN.UTF-8".to_string(),
        }
    }

    fn clone_spec(snapshot: serde_json::Value) -> IsoSpec {
        IsoSpec {
            variant: IsoVariant::Clone {
                config_snapshot: snapshot,
            },
            base_image: "x".into(),
            components: vec!["osd".into()],
            ubuntu_version: "24.04".to_string(),
            arch: "aarch64".into(),
            locale: "en_US.UTF-8".into(),
        }
    }

    fn valid_target() -> InstallTarget {
        InstallTarget {
            disks: vec!["/dev/sda".to_string()],
            zfs_raid_level: None,
            root_password_hash: "$6$rounds=...$hash".to_string(),
            admin_user: "admin".to_string(),
            network: json!({"mode": "dhcp"}),
            locale: "zh_CN.UTF-8".to_string(),
        }
    }

    // —— MockIsoBuilder ——

    #[tokio::test]
    async fn mock_build_completes() {
        let b = MockIsoBuilder::new();
        let task = b.build(std_spec()).await.unwrap();
        let s = b.status(&task).await;
        assert!(matches!(s, IsoBuildStatus::Completed(_)));
    }

    #[tokio::test]
    async fn mock_build_error_injected() {
        let b = MockIsoBuilder::new().with_build_error(IsoError::BuildFailed("boom".into()));
        let err = b.build(std_spec()).await.unwrap_err();
        assert!(matches!(err, IsoError::BuildFailed(_)));
        // 第二次无错误（错误已消费）
        let task = b.build(std_spec()).await.unwrap();
        let s = b.status(&task).await;
        assert!(matches!(s, IsoBuildStatus::Completed(_)));
    }

    #[tokio::test]
    async fn mock_build_invalid_spec() {
        let b = MockIsoBuilder::new();
        let mut s = std_spec();
        s.arch = "mips".into();
        assert!(b.build(s).await.is_err());
    }

    #[tokio::test]
    async fn mock_status_unknown() {
        let b = MockIsoBuilder::new();
        let s = b.status(&TaskId::new()).await;
        assert!(matches!(s, IsoBuildStatus::Failed { .. }));
    }

    #[tokio::test]
    async fn mock_verify_default_true() {
        let b = MockIsoBuilder::new();
        assert!(b.verify(Path::new("/tmp/x"), "any").await.unwrap());
    }

    #[tokio::test]
    async fn mock_verify_configurable_false() {
        let b = MockIsoBuilder::new().with_verify_result(false);
        assert!(!b.verify(Path::new("/tmp/x"), "any").await.unwrap());
    }

    #[tokio::test]
    async fn mock_initial_status_preset() {
        let task = TaskId::new();
        let b = MockIsoBuilder::new().with_initial_status(
            task,
            IsoBuildStatus::Building {
                step: "xorriso".to_string(),
                progress: 0.5,
            },
        );
        match b.status(&task).await {
            IsoBuildStatus::Building { step, progress } => {
                assert_eq!(step, "xorriso");
                assert!((progress - 0.5).abs() < 1e-6);
            }
            other => panic!("应是 Building，得到 {other:?}"),
        }
    }

    #[tokio::test]
    async fn mock_build_clone_sanitizes() {
        let b = MockIsoBuilder::new();
        let task = b
            .build(clone_spec(json!({"password": "leak", "host": "n"})))
            .await
            .unwrap();
        assert!(matches!(
            b.status(&task).await,
            IsoBuildStatus::Completed(_)
        ));
    }

    #[tokio::test]
    async fn mock_build_deterministic_results() {
        let b = MockIsoBuilder::new();
        let t1 = b.build(std_spec()).await.unwrap();
        let t2 = b.build(std_spec()).await.unwrap();
        // 两次 build 的产物路径/size 应递增（计数器）
        let r1 = match b.status(&t1).await {
            IsoBuildStatus::Completed(r) => r,
            _ => panic!("应是 Completed"),
        };
        let r2 = match b.status(&t2).await {
            IsoBuildStatus::Completed(r) => r,
            _ => panic!("应是 Completed"),
        };
        assert_ne!(r1.size_bytes, r2.size_bytes);
        assert_ne!(r1.sha256, r2.sha256);
    }

    // —— MockInstaller ——

    #[tokio::test]
    async fn mock_detect_default_report() {
        let inst = MockInstaller::new();
        let r = inst.detect_hardware().await.unwrap();
        assert_eq!(r.cpu, "Mock CPU");
        assert!(r.kvm_support);
        assert!(!r.disks.is_empty());
        assert_eq!(r.memory_gb, 16);
    }

    #[tokio::test]
    async fn mock_install_default_report() {
        let inst = MockInstaller::new();
        let r = inst
            .install(Path::new("/tmp/x"), valid_target())
            .await
            .unwrap();
        assert_eq!(r.pool_created.as_deref(), Some("tank"));
        assert!(!r.installed_components.is_empty());
        assert!(r
            .post_install_actions
            .iter()
            .any(|a| a.contains("重设 root 密码")));
    }

    #[tokio::test]
    async fn mock_install_admin_injected() {
        let inst = MockInstaller::new();
        let mut t = valid_target();
        t.admin_user = "ops".to_string();
        let r = inst.install(Path::new("/tmp/x"), t).await.unwrap();
        assert!(r
            .post_install_actions
            .iter()
            .any(|a| a.contains("初始化管理员用户: ops")));
    }

    #[tokio::test]
    async fn mock_install_error_injected() {
        let inst = MockInstaller::new().with_install_error(IsoError::InstallFailed("gone".into()));
        let err = inst
            .install(Path::new("/tmp/x"), valid_target())
            .await
            .unwrap_err();
        assert!(matches!(err, IsoError::InstallFailed(_)));
    }

    #[tokio::test]
    async fn mock_install_validates_target() {
        let inst = MockInstaller::new();
        let mut t = valid_target();
        t.disks.clear();
        assert!(inst.install(Path::new("/tmp/x"), t).await.is_err());
    }

    #[tokio::test]
    async fn mock_install_skip_validation() {
        let inst = MockInstaller::new().skip_target_validation();
        let mut t = valid_target();
        t.disks.clear();
        t.root_password_hash = String::new();
        let r = inst.install(Path::new("/tmp/x"), t).await.unwrap();
        assert_eq!(r.pool_created.as_deref(), Some("tank"));
    }

    #[tokio::test]
    async fn mock_custom_hardware() {
        let hw = HardwareReport {
            cpu: "Custom".to_string(),
            memory_gb: 64,
            disks: vec![],
            nics: vec![],
            kvm_support: false,
            warnings: vec!["custom".to_string()],
        };
        let inst = MockInstaller::new().with_hardware(hw);
        let r = inst.detect_hardware().await.unwrap();
        assert_eq!(r.memory_gb, 64);
        assert!(!r.kvm_support);
        assert_eq!(r.warnings, vec!["custom".to_string()]);
    }

    #[tokio::test]
    async fn mock_custom_install_report() {
        let report = InstallReport {
            installed_components: vec!["only-me".to_string()],
            pool_created: None,
            duration_secs: 42,
            post_install_actions: vec![],
            commands: Vec::new(),
        };
        let inst = MockInstaller::new().with_install_report(report);
        let r = inst
            .install(Path::new("/tmp/x"), valid_target())
            .await
            .unwrap();
        assert_eq!(r.installed_components, vec!["only-me".to_string()]);
        assert_eq!(r.duration_secs, 42);
    }
}
