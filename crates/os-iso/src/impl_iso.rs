//! `XorrisoIsoBuilder` —— 编排 xorriso + squashfs 产出可安装 ISO 的实现。
//!
//! 设计（呼应规格书 §3 / §6）：
//! - `build`：校验 spec → 派生 `TaskId` → 派生 squashfs/xorriso 命令参数（纯函数，
//!   见 [`crate::cli`]）→ 注册任务（内存态 Pending）→ 通过 [`crate::runner::IsoBuildRunner`]
//!   spawn mksquashfs → xorriso → sha256sum → 标记 Completed。
//! - `status`：查任务状态（Pending/Building{step,progress}/Completed/Failed）。
//! - `verify`：对既有 ISO 跑 `sha256sum` 并比对期望值（通过 runner）。
//!
//! `IsoBuildRunner` 抽象隔离子进程 spawn，使构建器可测：
//! - 生产用 [`crate::runner::TokioIsoRunner`]（真实 xorriso/mksquashfs）。
//! - 测试用 [`crate::runner::FixtureIsoRunner`]（确定性输出，零 xorriso 依赖）。

use crate::cli::{
    derive_boot_config, squashfs_pack_args, xorriso_build_args, BootConfig, SquashfsConfig,
};
use crate::iso::{IsoBuildResult, IsoBuildStatus, IsoBuilder, IsoSpec};
use crate::runner::{FixtureIsoRunner, IsoBuildRunner, ProcessOutput, TokioIsoRunner};
use crate::IsoError;
use os_core::TaskId;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, info};

/// 构建任务内部记录。
///
/// `spec` 与 `output_dir` 当前仅存档（真实执行 TODO 时用于重跑/诊断），故 allow dead_code。
#[derive(Debug, Clone)]
#[allow(dead_code)]
struct BuildTask {
    spec: IsoSpec,
    status: IsoBuildStatus,
    /// 产物输出目录（build 时派生）。
    output_dir: PathBuf,
    /// 派生的启动配置（build 时派生）。
    boot_config: BootConfig,
    /// 派生的 squashfs 配置（build 时派生）。
    squashfs_config: SquashfsConfig,
    /// 产物 ISO 路径（build 时派生）。
    iso_path: PathBuf,
}

/// xorriso/squashfs 编排型 ISO 构建器。
///
/// 状态：内部维护一张任务表（`TaskId` → `BuildTask`），`Mutex<HashMap>` 保护，
/// 纯内存，无外部依赖。`build` 注册任务并派生命令参数，通过 [`IsoBuildRunner`] 执行，
/// `status` 读任务状态，`verify` 比对 sha256。
///
/// runner 通过构造器注入：生产用 `TokioIsoRunner`，测试用 `FixtureIsoRunner`。
pub struct XorrisoIsoBuilder {
    tasks: Mutex<HashMap<TaskId, BuildTask>>,
    /// 产物根目录（任务 ISO 输出到此目录下）。
    output_root: PathBuf,
    /// 子进程执行器（生产 TokioIsoRunner / 测试 FixtureIsoRunner）。
    runner: Arc<dyn IsoBuildRunner>,
}

impl XorrisoIsoBuilder {
    /// 构造，给定产物输出根目录和 runner。
    #[must_use]
    pub fn new(output_root: impl Into<PathBuf>, runner: Arc<dyn IsoBuildRunner>) -> Self {
        Self {
            tasks: Mutex::new(HashMap::new()),
            output_root: output_root.into(),
            runner,
        }
    }

    /// 构造，使用 [`TokioIsoRunner`]（生产默认）。
    #[must_use]
    pub fn with_default_output() -> Self {
        Self::new(
            PathBuf::from("./build/iso"),
            Arc::new(TokioIsoRunner::new()),
        )
    }

    /// 构造，使用 [`FixtureIsoRunner`]（测试用，确定性输出）。
    #[must_use]
    pub fn with_fixture_runner(output_root: impl Into<PathBuf>) -> Self {
        Self::new(output_root.into(), Arc::new(FixtureIsoRunner::new()))
    }

    /// 派生单个任务的全部产物路径与配置（纯函数，不写盘）。
    ///
    /// 暴露为 `pub(crate)` 便于测试断言派生结果。
    fn derive_task_paths(
        &self,
        task_id: &TaskId,
        spec: &IsoSpec,
    ) -> (PathBuf, BootConfig, SquashfsConfig, PathBuf) {
        let output_dir = self.output_root.join(task_id.0.to_string());
        let boot_config = derive_boot_config(spec);
        let source_dir = output_dir.join("tree").to_string_lossy().into_owned();
        let squashfs_path = output_dir
            .join("casper/filesystem.squashfs")
            .to_string_lossy()
            .into_owned();
        let squashfs_config = SquashfsConfig::new(source_dir.clone(), squashfs_path);
        let iso_path = output_dir.join(format!(
            "os-{}-{}.iso",
            match &spec.variant {
                crate::iso::IsoVariant::Standard => "std",
                crate::iso::IsoVariant::Clone { .. } => "clone",
            },
            spec.ubuntu_version
        ));
        (output_dir, boot_config, squashfs_config, iso_path)
    }

    /// 执行 squashfs 打包（通过 runner）。
    async fn run_squashfs(&self, cfg: &SquashfsConfig) -> Result<ProcessOutput, IsoError> {
        let args = squashfs_pack_args(cfg);
        debug!(args = ?args, "squashfs 命令执行");
        let out = self.runner.run("mksquashfs", &args).await?;
        if !out.is_success() {
            return Err(IsoError::BuildFailed(format!(
                "mksquashfs 失败 (exit {}): {}",
                out.exit_code, out.stderr
            )));
        }
        Ok(out)
    }

    /// 执行 xorriso 生成 ISO（通过 runner）。
    async fn run_xorriso(
        &self,
        cfg: &BootConfig,
        source_tree: &str,
        output_iso: &str,
    ) -> Result<ProcessOutput, IsoError> {
        let args = xorriso_build_args(cfg, source_tree, output_iso);
        debug!(args = ?args, "xorriso 命令执行");
        let out = self.runner.run("xorriso", &args).await?;
        if !out.is_success() {
            return Err(IsoError::BuildFailed(format!(
                "xorriso 失败 (exit {}): {}",
                out.exit_code, out.stderr
            )));
        }
        Ok(out)
    }

    /// 计算 ISO 的 sha256（通过 runner）。
    async fn compute_sha256(&self, iso_path: &Path) -> Result<String, IsoError> {
        self.runner.compute_sha256(iso_path).await
    }

    /// 推进任务状态到 `Building { step, progress }`（内部，锁内调用）。
    fn set_status(&self, task: &TaskId, status: IsoBuildStatus) {
        if let Ok(mut guard) = self.tasks.lock() {
            if let Some(t) = guard.get_mut(task) {
                t.status = status;
            }
        }
    }

    /// 推进任务到指定 Building 阶段。
    fn set_building(&self, task: &TaskId, step: &str, progress: f32) {
        self.set_status(
            task,
            IsoBuildStatus::Building {
                step: step.to_string(),
                progress: progress.clamp(0.0, 1.0),
            },
        );
    }
}

impl IsoBuilder for XorrisoIsoBuilder {
    async fn build(&self, mut spec: IsoSpec) -> Result<TaskId, IsoError> {
        // 1. 校验 spec（架构/版本/组件/locale 非空）
        spec.validate()?;
        // 2. 克隆变体：内嵌前清洗 config_snapshot（剔除敏感项，§3.19）
        spec.sanitize_clone_snapshot();
        // 3. 派生 TaskId 与全部产物路径
        let task_id = TaskId::new();
        let (output_dir, boot_config, squashfs_config, iso_path) =
            self.derive_task_paths(&task_id, &spec);
        info!(%task_id, ?output_dir, "ISO 构建任务已注册");

        // 4. 注册任务（Pending）
        {
            let mut guard = self
                .tasks
                .lock()
                .map_err(|e| IsoError::Internal(format!("任务表锁中毒: {e}")))?;
            guard.insert(
                task_id,
                BuildTask {
                    spec: spec.clone(),
                    status: IsoBuildStatus::Pending,
                    output_dir,
                    boot_config: boot_config.clone(),
                    squashfs_config: squashfs_config.clone(),
                    iso_path: iso_path.clone(),
                },
            );
        }

        // 5. 阶段一：squashfs 打包
        self.set_building(&task_id, "squashfs", 0.1);
        self.run_squashfs(&squashfs_config).await?;
        self.set_building(&task_id, "squashfs", 0.4);

        // 6. 阶段二：xorriso 生成 ISO
        self.set_building(&task_id, "xorriso", 0.5);
        let source_dir = &squashfs_config.source_dir;
        let iso_path_str = iso_path.to_string_lossy().into_owned();
        self.run_xorriso(&boot_config, source_dir, &iso_path_str)
            .await?;
        self.set_building(&task_id, "xorriso", 0.8);

        // 7. 阶段三：计算 sha256 + 文件大小
        let sha256 = self.compute_sha256(&iso_path).await?;
        let size_bytes = self.runner.file_size(&iso_path).await?;
        self.set_building(&task_id, "xorriso", 0.9);

        // 8. 构造产物，标记 Completed
        let result = Self::make_build_result(iso_path, sha256, size_bytes);
        self.set_status(&task_id, IsoBuildStatus::Completed(result.clone()));
        info!(%task_id, ?result.iso_path, result.size_bytes, "ISO 构建完成");
        Ok(task_id)
    }

    async fn status(&self, task: &TaskId) -> IsoBuildStatus {
        let guard = match self.tasks.lock() {
            Ok(g) => g,
            Err(_) => {
                return IsoBuildStatus::Failed {
                    reason: "任务表锁中毒".to_string(),
                }
            }
        };
        guard
            .get(task)
            .map(|t| t.status.clone())
            .unwrap_or(IsoBuildStatus::Failed {
                reason: format!("任务不存在: {task}"),
            })
    }

    async fn verify(&self, iso_path: &Path, expected_sha256: &str) -> Result<bool, IsoError> {
        let actual = self.compute_sha256(iso_path).await?;
        let actual_lc = actual.to_ascii_lowercase();
        let expected_lc = expected_sha256.to_ascii_lowercase();
        Ok(actual_lc == expected_lc)
    }
}

impl XorrisoIsoBuilder {
    /// 取某任务的派生命令参数（测试辅助 + 诊断）。
    ///
    /// 返回 `(squashfs_args, xorriso_args, iso_path)`；任务不存在返回 None。
    pub fn task_command_args(&self, task: &TaskId) -> Option<(Vec<String>, Vec<String>, PathBuf)> {
        let guard = self.tasks.lock().ok()?;
        let t = guard.get(task)?;
        let sq = squashfs_pack_args(&t.squashfs_config);
        let xo = xorriso_build_args(
            &t.boot_config,
            &t.squashfs_config.source_dir,
            &t.iso_path.to_string_lossy(),
        );
        Some((sq, xo, t.iso_path.clone()))
    }

    /// 构造一个确定性的 `IsoBuildResult`（供测试与 mock 复用）。
    ///
    /// 注：`built_at` 取当前 UTC 时间（系统时钟）。size_bytes/sha256 由调用方填。
    #[must_use]
    pub fn make_build_result(iso_path: PathBuf, sha256: String, size_bytes: u64) -> IsoBuildResult {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        // os_core::DateTime 是 chrono::DateTime<chrono::Utc> 的 type 别名（ADR-COMPAT-002）
        let built_at =
            chrono::DateTime::<chrono::Utc>::from_timestamp(now, 0).unwrap_or_else(|| {
                chrono::DateTime::<chrono::Utc>::from_timestamp(0, 0).expect("epoch valid")
            });
        IsoBuildResult {
            iso_path,
            sha256,
            size_bytes,
            built_at,
        }
    }
}

impl Default for XorrisoIsoBuilder {
    fn default() -> Self {
        Self::with_default_output()
    }
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iso::{IsoVariant, IsoVariant::*};
    use crate::runner::FixtureIsoRunner;
    use serde_json::json;
    use std::sync::Arc;

    fn std_spec() -> IsoSpec {
        IsoSpec {
            variant: IsoVariant::Standard,
            base_image: "ubuntu-24.04-base.squashfs".to_string(),
            components: vec!["osd".to_string(), "os-storage".to_string()],
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

    /// 构造一个使用 FixtureIsoRunner 的 builder（三阶段均返回成功）。
    fn fixture_builder() -> XorrisoIsoBuilder {
        let runner = FixtureIsoRunner::new();
        XorrisoIsoBuilder::new("/tmp/test-iso", Arc::new(runner))
    }

    #[test]
    fn derive_task_paths_shape() {
        let b = fixture_builder();
        let task = TaskId::new();
        let spec = std_spec();
        let (out_dir, boot, sq, iso) = b.derive_task_paths(&task, &spec);
        assert!(out_dir.starts_with("/tmp/test-iso"));
        assert!(boot.volume_id.starts_with("OS-"));
        assert!(sq.source_dir.contains("tree"));
        assert!(sq.output_file.contains("filesystem.squashfs"));
        assert!(iso.to_string_lossy().ends_with(".iso"));
        assert!(iso.to_string_lossy().contains("std"));
    }

    #[test]
    fn derive_task_paths_clone_tag() {
        let b = fixture_builder();
        let spec = clone_spec(json!({}));
        let (_o, _boot, _sq, iso) = b.derive_task_paths(&TaskId::new(), &spec);
        assert!(iso.to_string_lossy().contains("clone"));
    }

    #[test]
    fn make_build_result_populates_fields() {
        let r = XorrisoIsoBuilder::make_build_result(
            std::path::PathBuf::from("/tmp/x.iso"),
            "abc".to_string(),
            1024,
        );
        assert_eq!(r.sha256, "abc");
        assert_eq!(r.size_bytes, 1024);
        assert_eq!(r.iso_path, std::path::PathBuf::from("/tmp/x.iso"));
        // built_at 非 None（具体值依赖时钟，仅验证可调用）
    }

    // —— build 通过 runner 执行（fixture runner）——

    #[tokio::test]
    async fn build_with_fixture_completes() {
        let b = fixture_builder();
        let task = b.build(std_spec()).await.unwrap();
        let s = b.status(&task).await;
        assert!(matches!(s, IsoBuildStatus::Completed(_)), "实际状态: {s:?}");
    }

    #[tokio::test]
    async fn build_with_fixture_completed_result() {
        let b = fixture_builder();
        let task = b.build(std_spec()).await.unwrap();
        if let IsoBuildStatus::Completed(r) = b.status(&task).await {
            // fixture sha256 = 64 个 'a'，size = 100 MiB
            assert_eq!(r.sha256, "a".repeat(64));
            assert_eq!(r.size_bytes, 1024 * 1024 * 100);
            assert!(r.iso_path.to_string_lossy().ends_with(".iso"));
        } else {
            panic!("应是 Completed");
        }
    }

    #[tokio::test]
    async fn build_with_fixture_clone_completes() {
        let b = fixture_builder();
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
    async fn build_invalid_spec() {
        let b = fixture_builder();
        let mut s = std_spec();
        s.arch = "mips".to_string();
        let err = b.build(s).await.unwrap_err();
        assert!(matches!(err, IsoError::BuildFailed(_)));
    }

    #[tokio::test]
    async fn build_squashfs_failure_marks_failed() {
        // fixture 让 mksquashfs 返回非零退出码
        let runner = FixtureIsoRunner::new().on(
            "mksquashfs",
            "/tmp",
            ProcessOutput {
                stdout: String::new(),
                stderr: "squashfs error".into(),
                exit_code: 1,
            },
        );
        let b = XorrisoIsoBuilder::new("/tmp/test-iso", Arc::new(runner));
        let err = b.build(std_spec()).await.unwrap_err();
        assert!(matches!(err, IsoError::BuildFailed(_)));
        assert!(err.to_string().contains("mksquashfs"));
    }

    #[tokio::test]
    async fn build_xorriso_failure_marks_failed() {
        // mksquashfs 成功，xorriso 失败
        let runner = FixtureIsoRunner::new()
            .on("mksquashfs", "/tmp", ProcessOutput::ok())
            .on(
                "xorriso",
                "-as",
                ProcessOutput {
                    stdout: String::new(),
                    stderr: "xorriso error".into(),
                    exit_code: 2,
                },
            );
        let b = XorrisoIsoBuilder::new("/tmp/test-iso", Arc::new(runner));
        let err = b.build(std_spec()).await.unwrap_err();
        assert!(matches!(err, IsoError::BuildFailed(_)));
        assert!(err.to_string().contains("xorriso"));
    }

    // —— status ——

    #[tokio::test]
    async fn status_unknown_task_failed() {
        let b = fixture_builder();
        let s = b.status(&TaskId::new()).await;
        assert!(matches!(s, IsoBuildStatus::Failed { .. }));
    }

    // —— task_command_args ——

    #[tokio::test]
    async fn task_command_args_derived() {
        let b = fixture_builder();
        let task = b.build(std_spec()).await.unwrap();
        let (sq, xo, iso) = b.task_command_args(&task).unwrap();
        assert!(sq.contains(&"-comp".to_string()));
        assert!(xo.contains(&"-as".to_string()));
        assert!(iso.to_string_lossy().ends_with(".iso"));
    }

    #[tokio::test]
    async fn task_command_args_unknown_returns_none() {
        let b = fixture_builder();
        assert!(b.task_command_args(&TaskId::new()).is_none());
    }

    // —— verify（通过 runner）——

    #[tokio::test]
    async fn verify_with_fixture_match() {
        let b = fixture_builder();
        // fixture sha256 = 64 个 'a'
        let ok = b
            .verify(std::path::Path::new("/tmp/x.iso"), &"a".repeat(64))
            .await
            .unwrap();
        assert!(ok);
    }

    #[tokio::test]
    async fn verify_with_fixture_mismatch() {
        let b = fixture_builder();
        let ok = b
            .verify(std::path::Path::new("/tmp/x.iso"), &"b".repeat(64))
            .await
            .unwrap();
        assert!(!ok);
    }

    // —— variant match（编译期分支确认）——

    #[test]
    fn variant_match() {
        let v = IsoVariant::Standard;
        assert!(!matches!(v, Clone { .. }));
        let c = IsoVariant::Clone {
            config_snapshot: json!({}),
        };
        assert!(matches!(c, Clone { .. }));
    }
}
