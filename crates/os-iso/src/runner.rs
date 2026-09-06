//! ISO 构建执行抽象——隔离子进程 spawn，使 `XorrisoIsoBuilder` 可测。
//!
//! 设计与 `os-storage::CommandRunner` 同构：
//! - [`IsoBuildRunner`] trait：抽象「spawn 程序 + 等待 + 解析输出」。
//! - [`TokioIsoRunner`]：生产实现，`tokio::process::Command` spawn 真实 xorriso/mksquashfs/sha256sum。
//! - [`FixtureIsoRunner`]：测试实现，返回确定性输出（零 xorriso 依赖）。
//!
//! 真实构建需 xorriso + squashfs-tools 系统包；测试用 `FixtureIsoRunner`；
//! 真实 spawn 测试用 `/bin/echo` 等无害命令验证机制，标 `#[ignore]`（需 apt install xorriso squashfs-tools）。

use crate::IsoError;
use std::path::Path;

/// 子进程执行结果（与 `os_core::CommandOutput` 同构，本 crate 独立定义避免跨 crate 依赖）。
#[derive(Debug, Clone)]
pub struct ProcessOutput {
    /// stdout（UTF-8 解码后的字符串）。
    pub stdout: String,
    /// stderr（UTF-8 解码后的字符串）。
    pub stderr: String,
    /// 退出码（0 = 成功；-1 = 被信号杀）。
    pub exit_code: i32,
}

impl ProcessOutput {
    /// 成功、空输出的便捷构造。
    #[must_use]
    pub fn ok() -> Self {
        Self {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
        }
    }

    /// 成功并携带 stdout。
    #[must_use]
    pub fn ok_stdout(stdout: impl Into<String>) -> Self {
        Self {
            stdout: stdout.into(),
            stderr: String::new(),
            exit_code: 0,
        }
    }

    /// 是否成功（退出码 0）。
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.exit_code == 0
    }
}

/// ISO 构建执行器抽象——隔离子进程 spawn。
///
/// 与 `os-storage::CommandRunner` 同构：`run` 执行 `<program> <args...>`，
/// 返回 stdout/stderr/退出码。`XorrisoIsoBuilder` 通过此 trait 编排
/// mksquashfs / xorriso / sha256sum 三阶段构建。
///
/// 生产用 [`TokioIsoRunner`]；测试用 [`FixtureIsoRunner`]。
#[async_trait::async_trait]
pub trait IsoBuildRunner: Send + Sync {
    /// 执行 `<program> <args...>`，返回 stdout/stderr/退出码。
    ///
    /// 失败（spawn 失败/进程异常退出）转 `IsoError::BuildFailed`。
    async fn run(&self, program: &str, args: &[String]) -> Result<ProcessOutput, IsoError>;

    /// 计算文件的 SHA256 摘要（小写 hex 64 位）。
    ///
    /// 默认实现：spawn `sha256sum <file>` 并解析输出。
    /// 生产用 [`TokioIsoRunner`]（真实 sha256sum）；测试用 [`FixtureIsoRunner`]（固定值）。
    async fn compute_sha256(&self, file: &Path) -> Result<String, IsoError> {
        let args = vec![file.to_string_lossy().into_owned()];
        let out = self.run("sha256sum", &args).await?;
        if !out.is_success() {
            return Err(IsoError::VerificationFailed(format!(
                "sha256sum 失败: {}",
                out.stderr
            )));
        }
        // 解析输出形如 "<hash>  <file>\n"
        let parsed = crate::cli::parse_sha256sum_output(&out.stdout).ok_or_else(|| {
            IsoError::VerificationFailed(format!("sha256sum 输出解析失败: {}", out.stdout))
        })?;
        Ok(parsed)
    }

    /// 获取 ISO 文件大小（字节）。
    ///
    /// 默认实现：`std::fs::metadata`。fixture 可覆写。
    async fn file_size(&self, path: &Path) -> Result<u64, IsoError> {
        std::fs::metadata(path)
            .map(|m| m.len())
            .map_err(|e| IsoError::Io(format!("无法获取文件大小 {}: {}", path.display(), e)))
    }
}

// ----------------------------------------------------------------------------
// TokioIsoRunner —— 生产实现（真实 spawn）
// ----------------------------------------------------------------------------

/// 生产用 ISO 构建执行器——`tokio::process::Command` spawn 真实子进程。
///
/// xorriso / mksquashfs / sha256sum 必须在 `$PATH`。
pub struct TokioIsoRunner;

impl TokioIsoRunner {
    /// 构造。
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for TokioIsoRunner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl IsoBuildRunner for TokioIsoRunner {
    async fn run(&self, program: &str, args: &[String]) -> Result<ProcessOutput, IsoError> {
        use std::process::Stdio;
        use tokio::process::Command;
        use tracing::{debug, warn};

        debug!(program, args = ?args, "spawn 子进程");
        let output = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|e| IsoError::BuildFailed(format!("spawn {} 失败: {}", program, e)))?;

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let exit_code = output.status.code().unwrap_or(-1);

        if !stdout.is_empty() {
            debug!(program, %stdout, "子进程 stdout");
        }
        if !stderr.is_empty() {
            // xorriso 偶尔在 stderr 打进度信息而非错误；仅 warn 非零退出
            if exit_code != 0 {
                warn!(program, %stderr, exit_code, "子进程 stderr（非零退出）");
            } else {
                debug!(program, %stderr, "子进程 stderr（零退出）");
            }
        }

        Ok(ProcessOutput {
            stdout,
            stderr,
            exit_code,
        })
    }
}

// ----------------------------------------------------------------------------
// FixtureIsoRunner —— 测试实现（确定性输出，零 xorriso 依赖）
// ----------------------------------------------------------------------------

/// 测试用 ISO 构建执行器——返回确定性输出，不 spawn 任何子进程。
///
/// 用法：通过 `on` 注册 fixture（program + 匹配条件 → 固定输出），
/// 或用 `default_output` 设置兜底输出。
pub struct FixtureIsoRunner {
    fixtures: std::sync::Mutex<Vec<FixtureEntry>>,
    /// 未匹配任何 fixture 时的默认输出（默认成功空输出）。
    default: ProcessOutput,
}

/// 单条 fixture 规则：program 名 + args 包含子串 → 固定输出。
struct FixtureEntry {
    program: String,
    args_contains: String,
    output: ProcessOutput,
}

impl FixtureIsoRunner {
    /// 构造空 fixture（默认成功空输出）。
    #[must_use]
    pub fn new() -> Self {
        Self {
            fixtures: std::sync::Mutex::new(Vec::new()),
            default: ProcessOutput::ok(),
        }
    }

    /// 注册一条 fixture 规则（链式调用）。
    ///
    /// 当 `program` 匹配且 `args` 包含 `args_contains` 子串时返回 `output`。
    #[must_use]
    pub fn on(
        mut self,
        program: impl Into<String>,
        args_contains: impl Into<String>,
        output: ProcessOutput,
    ) -> Self {
        self.fixtures
            .get_mut()
            .expect("fixture lock poisoned")
            .push(FixtureEntry {
                program: program.into(),
                args_contains: args_contains.into(),
                output,
            });
        self
    }

    /// 设置未匹配 fixture 时的默认输出。
    #[must_use]
    pub fn with_default(mut self, output: ProcessOutput) -> Self {
        self.default = output;
        self
    }
}

impl Default for FixtureIsoRunner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl IsoBuildRunner for FixtureIsoRunner {
    async fn run(&self, program: &str, args: &[String]) -> Result<ProcessOutput, IsoError> {
        let fixtures = self.fixtures.lock().expect("fixture lock poisoned");
        let args_str = args.join(" ");
        for entry in fixtures.iter() {
            if entry.program == program && args_str.contains(&entry.args_contains) {
                return Ok(entry.output.clone());
            }
        }
        // 未匹配 → 返回默认输出
        Ok(self.default.clone())
    }

    async fn compute_sha256(&self, _file: &Path) -> Result<String, IsoError> {
        // fixture 返回确定性哈希（64 个 'a'）
        Ok("a".repeat(64))
    }

    async fn file_size(&self, _path: &Path) -> Result<u64, IsoError> {
        // fixture 返回确定性大小
        Ok(1024 * 1024 * 100) // 100 MiB
    }
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // —— ProcessOutput ——

    #[test]
    fn process_output_ok_is_success() {
        let o = ProcessOutput::ok();
        assert!(o.is_success());
        assert_eq!(o.exit_code, 0);
        assert!(o.stdout.is_empty());
    }

    #[test]
    fn process_output_ok_stdout() {
        let o = ProcessOutput::ok_stdout("hello");
        assert!(o.is_success());
        assert_eq!(o.stdout, "hello");
    }

    #[test]
    fn process_output_nonzero_not_success() {
        let o = ProcessOutput {
            stdout: String::new(),
            stderr: "err".into(),
            exit_code: 1,
        };
        assert!(!o.is_success());
    }

    #[test]
    fn process_output_signal_killed() {
        let o = ProcessOutput {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: -1,
        };
        assert!(!o.is_success());
    }

    // —— FixtureIsoRunner ——

    #[tokio::test]
    async fn fixture_default_ok() {
        let r = FixtureIsoRunner::new();
        let o = r
            .run("mksquashfs", &["/src".into(), "/out".into()])
            .await
            .unwrap();
        assert!(o.is_success());
    }

    #[tokio::test]
    async fn fixture_matches_program_and_args() {
        let r = FixtureIsoRunner::new()
            .on(
                "mksquashfs",
                "/src",
                ProcessOutput::ok_stdout("squashfs-done"),
            )
            .on("xorriso", "-as", ProcessOutput::ok_stdout("xorriso-done"));
        let sq = r
            .run("mksquashfs", &["/src".into(), "/out".into()])
            .await
            .unwrap();
        assert_eq!(sq.stdout, "squashfs-done");
        let xo = r
            .run("xorriso", &["-as".into(), "mkisofs".into()])
            .await
            .unwrap();
        assert_eq!(xo.stdout, "xorriso-done");
    }

    #[tokio::test]
    async fn fixture_no_match_returns_default() {
        let r = FixtureIsoRunner::new().on("zpool", "list", ProcessOutput::ok_stdout("pool"));
        let o = r.run("unknown_cmd", &["arg1".into()]).await.unwrap();
        assert!(o.is_success());
        assert!(o.stdout.is_empty());
    }

    #[tokio::test]
    async fn fixture_default_override() {
        let r = FixtureIsoRunner::new().with_default(ProcessOutput {
            stdout: "fallback".into(),
            stderr: String::new(),
            exit_code: 2,
        });
        let empty_args: &[String] = &[];
        let o = r.run("anything", empty_args).await.unwrap();
        assert_eq!(o.exit_code, 2);
        assert_eq!(o.stdout, "fallback");
    }

    #[tokio::test]
    async fn fixture_compute_sha256_deterministic() {
        let r = FixtureIsoRunner::new();
        let h = r.compute_sha256(Path::new("/tmp/x.iso")).await.unwrap();
        assert_eq!(h, "a".repeat(64));
    }

    #[tokio::test]
    async fn fixture_file_size_deterministic() {
        let r = FixtureIsoRunner::new();
        let s = r.file_size(Path::new("/tmp/x.iso")).await.unwrap();
        assert_eq!(s, 1024 * 1024 * 100);
    }

    #[tokio::test]
    async fn fixture_multiple_entries_first_match_wins() {
        let r = FixtureIsoRunner::new()
            .on("xorriso", "-as", ProcessOutput::ok_stdout("first"))
            .on("xorriso", "-as", ProcessOutput::ok_stdout("second"));
        let o = r
            .run("xorriso", &["-as".into(), "mkisofs".into()])
            .await
            .unwrap();
        assert_eq!(o.stdout, "first");
    }

    // —— TokioIsoRunner: spawn 机制验证（无害命令 /bin/echo）——

    #[tokio::test]
    async fn tokio_runner_echo_success() {
        let r = TokioIsoRunner::new();
        let out = r.run("/bin/echo", &["hello world".into()]).await.unwrap();
        assert!(out.is_success());
        assert!(out.stdout.contains("hello world"));
    }

    #[tokio::test]
    async fn tokio_runner_echo_empty_args() {
        let r = TokioIsoRunner::new();
        let out = r.run("/bin/echo", &[]).await.unwrap();
        assert!(out.is_success());
        // echo with no args outputs a newline
        assert_eq!(out.stdout.trim(), "");
    }

    #[tokio::test]
    async fn tokio_runner_nonexistent_program_fails() {
        let r = TokioIsoRunner::new();
        let err = r
            .run("nonexistent_program_xyz_12345", &["arg".into()])
            .await;
        assert!(err.is_err());
        let err = err.unwrap_err();
        assert!(matches!(err, IsoError::BuildFailed(_)));
        assert!(err.to_string().contains("spawn"));
    }

    #[tokio::test]
    async fn tokio_runner_false_exits_nonzero() {
        let r = TokioIsoRunner::new();
        let out = r.run("/bin/false", &[]).await.unwrap();
        assert!(!out.is_success());
        assert_ne!(out.exit_code, 0);
    }

    #[tokio::test]
    async fn tokio_runner_sha256sum_on_real_file() {
        // 用 write 创建临时文件再算 sha256 ——验证 compute_sha256 端到端
        let dir = std::env::temp_dir().join("os_iso_runner_test");
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join("echo_test.txt");
        std::fs::write(&tmp, "test content for sha256").unwrap();
        let r = TokioIsoRunner::new();
        let hash = r.compute_sha256(&tmp).await.unwrap();
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        // 清理
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tokio_runner_file_size_real() {
        let dir = std::env::temp_dir().join("os_iso_runner_test_size");
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join("size_test.bin");
        std::fs::write(&tmp, vec![0u8; 12345]).unwrap();
        let r = TokioIsoRunner::new();
        let size = r.file_size(&tmp).await.unwrap();
        assert_eq!(size, 12345);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // —— #[ignore] 真实 xorriso 测试（需 apt install xorriso squashfs-tools）——

    #[tokio::test]
    #[ignore]
    async fn tokio_runner_real_xorriso_version() {
        let r = TokioIsoRunner::new();
        let out = r.run("xorriso", &["--version".into()]).await.unwrap();
        assert!(out.is_success());
        assert!(out.stdout.contains("xorriso"));
    }

    #[tokio::test]
    #[ignore]
    async fn tokio_runner_real_mksquashfs_version() {
        let r = TokioIsoRunner::new();
        let out = r.run("mksquashfs", &["--version".into()]).await.unwrap();
        assert!(out.is_success());
        assert!(out.stdout.contains("mksquashfs"));
    }
}
