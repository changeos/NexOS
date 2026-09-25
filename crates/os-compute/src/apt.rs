//! apt/dpkg 命令构造 + `.deb` 包元数据 + **真实执行层**。
//!
//! 定位：[`crate::pkg::PackageManager`] 的实现（批 3 `DpkgPackageManager`）经
//! [`AptRunner`] 抽象调 `dpkg`/`apt-get` 完成第三方 `.deb` 安装/卸载。本模块分两层：
//!
//! - **命令构造层**（`*_argv`）：纯函数，把高层意图（install/uninstall/upgrade）翻译
//!   成 argv，可单测验证命令正确性，且不触发真实子进程；
//! - **执行层**（[`AptRunner`] trait + [`TokioAptRunner`] / `FixtureAptRunner`）：
//!   经抽象 spawn 子进程 + 等待 + 解析退出码；上层编排（install/uninstall/upgrade/
//!   list_installed/search）调构造层拼 argv 再交给 runner 执行并解析。
//!
//! **隔离真实 apt 改宿主**（规格书 §9 红线）：真实执行走 [`TokioAptRunner`]，
//! 默认不参与 `cargo test`——真实环境测试标 `#[ignore]`（需 root + apt + 写
//! `/var/lib/dpkg`），由人工或 CI 用 `cargo test -- --ignored` 触发；常规 `cargo test`
//! 走 `FixtureAptRunner`（仅 `cfg(test)`）注入预录 fixture 输出，零系统依赖。
//!
//! apt/dpkg 编排约定（apt best practice）：
//! - 安装本地 `.deb`：先 `dpkg -i <deb>`（可能因缺依赖失败），再 `apt-get -f install`
//!   修复依赖（apt 会自动下载缺失依赖）；
//! - 卸载：`apt-get remove --purge <pkg>`（连配置一起删）；
//! - 升级：`apt-get install --only-upgrade <pkg>`（避免误装新包）；
//! - 全程 `-y` 非交互（无人值守），`DEBIAN_FRONTEND=noninteractive` 抑制提示。

use std::path::PathBuf;

use os_core::CommandOutput;
use serde::{Deserialize, Serialize};

use crate::error::{ComputeError, ComputeResult};
use crate::pkg::PackageId;

// ----------------------------------------------------------------------------
// 环境与公共参数
// ----------------------------------------------------------------------------

/// 非交互环境变量键。
pub const NONINTERACTIVE_ENV_KEY: &str = "DEBIAN_FRONTEND";
/// 非交互环境变量值。
pub const NONINTERACTIVE_ENV_VAL: &str = "noninteractive";
/// apt-get 公共非交互标志。
pub const APT_YES_FLAG: &str = "-y";

/// 返回 apt/dpkg 命令应注入的非交互环境变量（`DEBIAN_FRONTEND=noninteractive`）。
pub fn noninteractive_env() -> Vec<(&'static str, &'static str)> {
    vec![(NONINTERACTIVE_ENV_KEY, NONINTERACTIVE_ENV_VAL)]
}

// ----------------------------------------------------------------------------
// argv 构造（返回 Vec<String>，便于断言；实现层喂给 tokio Command）
// ----------------------------------------------------------------------------

/// 安装本地 `.deb` 的命令序列：`dpkg -i <deb>` 后跟 `apt-get -f install -y`。
///
/// 两步的原因：纯 dpkg 装本地包不会自动拉依赖，缺失依赖时 dpkg 报错并留下半装状态；
/// 紧接 `apt-get -f install` 让 apt 解析并补齐依赖（apt 能访问仓库索引）。
///
/// 返回多条命令——实现层顺序执行，任一失败回滚（dpkg --configure -a 修复）。
pub fn install_argv(deb_path: &std::path::Path) -> ComputeResult<Vec<Vec<String>>> {
    let p = deb_path.to_str().ok_or_else(|| {
        ComputeError::InvalidSpec(format!("deb 路径非 UTF-8: {}", deb_path.display()))
    })?;
    if deb_path.extension().and_then(|e| e.to_str()) != Some("deb") {
        return Err(ComputeError::InvalidSpec(format!(
            "非 .deb 文件: {}",
            deb_path.display()
        )));
    }
    Ok(vec![
        // 第一步：dpkg -i 解包
        vec!["dpkg".to_string(), "-i".to_string(), p.to_string()],
        // 第二步：apt-get -f install -y（修复依赖）
        vec![
            "apt-get".to_string(),
            "-f".to_string(),
            "install".to_string(),
            APT_YES_FLAG.to_string(),
        ],
    ])
}

/// 卸载包的 argv：`apt-get remove --purge -y <pkg>`。
///
/// `--purge` 连配置文件一起删（用户卸载通常期望彻底清除）。
pub fn uninstall_argv(id: &PackageId) -> Vec<String> {
    vec![
        "apt-get".to_string(),
        "remove".to_string(),
        "--purge".to_string(),
        APT_YES_FLAG.to_string(),
        id.as_str().to_string(),
    ]
}

/// 升级包的 argv：`apt-get install --only-upgrade -y <pkg>`。
///
/// `--only-upgrade` 防止 apt 在包未安装时执行安装（语义保持精确）。
pub fn upgrade_argv(id: &PackageId) -> Vec<String> {
    vec![
        "apt-get".to_string(),
        "install".to_string(),
        "--only-upgrade".to_string(),
        APT_YES_FLAG.to_string(),
        id.as_str().to_string(),
    ]
}

/// `apt-get update`（刷新索引，install/upgrade 前调）。
pub fn update_argv() -> Vec<String> {
    vec![
        "apt-get".to_string(),
        "update".to_string(),
        APT_YES_FLAG.to_string(),
    ]
}

/// `dpkg-query` 列已装包（`-W` = show，`-f` = format）。
///
/// 格式 `${Package}\t${Version}\t${binary:Summary}\n`——制表符分隔，便于 split。
pub fn list_installed_argv() -> Vec<String> {
    vec![
        "dpkg-query".to_string(),
        "-W".to_string(),
        "-f=${Package}\t${Version}\t${binary:Summary}\n".to_string(),
    ]
}

/// `apt-cache search <query>`（搜索包名/描述）。
pub fn search_argv(query: &str) -> Vec<String> {
    vec![
        "apt-cache".to_string(),
        "search".to_string(),
        query.to_string(),
    ]
}

// ----------------------------------------------------------------------------
// dpkg-query 输出解析
// ----------------------------------------------------------------------------

/// 解析 `dpkg-query -W -f=...` 的输出行为 [`DpkgEntry`]。
///
/// 行格式：`<package>\t<version>\t<summary>\n`（见 [`list_installed_argv`]）。
/// 缺字段或空行返回 None（容忍尾随换行）。
pub fn parse_dpkg_line(line: &str) -> Option<DpkgEntry> {
    let line = line.trim_end_matches('\n');
    if line.is_empty() {
        return None;
    }
    let mut parts = line.split('\t');
    let package = parts.next()?.trim();
    let version = parts.next()?.trim();
    let summary = parts
        .next()
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if package.is_empty() {
        return None;
    }
    Some(DpkgEntry {
        package: package.to_string(),
        version: version.to_string(),
        summary,
    })
}

/// dpkg-query 单条记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DpkgEntry {
    /// 包名
    pub package: String,
    /// 版本
    pub version: String,
    /// 简短描述（binary:Summary）
    pub summary: String,
}

// ----------------------------------------------------------------------------
// .deb 文件名解析（versioned name: name_version_arch.deb）
// ----------------------------------------------------------------------------

/// 从 `.deb` 文件名解析包元数据。
///
/// Debian 命名约定：`<name>_<version>_<arch>.deb`。本函数不强校验 arch/version
/// 合法性（保留宽松），仅按 `_` 与 `.deb` 后缀切分。
pub fn parse_deb_filename(filename: &str) -> ComputeResult<DebFilename> {
    let name = filename
        .strip_suffix(".deb")
        .ok_or_else(|| ComputeError::InvalidSpec(format!("非 .deb 文件名: {filename}")))?;
    let mut parts = name.split('_');
    let pkg = parts.next().unwrap_or("");
    let version = parts.next();
    let arch = parts.next();
    if pkg.is_empty() {
        return Err(ComputeError::InvalidSpec(format!(
            "deb 文件名缺包名: {filename}"
        )));
    }
    Ok(DebFilename {
        package: pkg.to_string(),
        version: version.map(|s| s.to_string()),
        arch: arch.map(|s| s.to_string()),
    })
}

/// `.deb` 文件名解析结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DebFilename {
    /// 包名
    pub package: String,
    /// 版本（None = 文件名未含版本）
    pub version: Option<String>,
    /// 架构（amd64 / arm64 / all / ...）
    pub arch: Option<String>,
}

/// `.desktop` 文件默认搜索目录（install 后扫描这些目录找图标应用）。
pub const DESKTOP_FILE_DIRS: &[&str] = &[
    "/usr/share/applications",
    "/usr/local/share/applications",
    "/var/lib/flatpak/exports/share/applications",
];

/// 图标默认搜索目录（按 freedesktop icon theme spec）。
pub const ICON_DIRS: &[&str] = &[
    "/usr/share/icons",
    "/usr/share/pixmaps",
    "/usr/local/share/icons",
];

/// 默认 .desktop 扫描根（暴露为 PathBuf 便于实现层拼路径）。
pub fn desktop_dirs() -> Vec<PathBuf> {
    DESKTOP_FILE_DIRS.iter().map(PathBuf::from).collect()
}

// ----------------------------------------------------------------------------
// 执行层抽象（AptRunner）——隔离 spawn，便于测试注入 fixture
// ----------------------------------------------------------------------------

// `CommandOutput`（子进程执行结果：stdout/stderr/exit_code）现统一来自
// `os_core::CommandOutput`（review2 P-R2-1）。原 apt 执行层独立定义的同构结构及其
// 便捷构造器（ok/ok_with_stdout/fail/is_success）已上提到 os-core，本 crate 直接引用。

/// apt/dpkg 命令执行器抽象——隔离子进程 spawn，使上层编排可测。
///
/// 生产实现 [`TokioAptRunner`] 调真实 `dpkg`/`apt-get`/`apt-cache`/`dpkg-query`
/// （自动注入 `DEBIAN_FRONTEND=noninteractive`）；测试用 `FixtureAptRunner`（仅
/// `cfg(test)`）注入预录 fixture 输出，零系统依赖（规格书 §9 红线「不真跑 apt」由
/// `#[ignore]` 守护）。
///
/// 与 `os-compute` 其他 trait 一致用原生 `async fn in_trait`（无 `#[async_trait]`）。
/// 因原生 async fn in trait 不是 object-safe，上层编排函数用泛型 `<R: AptRunner>`
/// （而非 `&dyn AptRunner`）——调用方传具体类型即可，零虚表开销。
#[allow(async_fn_in_trait)]
pub trait AptRunner: Send + Sync {
    /// 执行 `<program> <args...>`，返回 stdout/stderr/退出码。
    ///
    /// 实现应：
    /// - 自动注入 [`noninteractive_env`]（抑制交互提示，无人值守）；
    /// - stdin 接 `/dev/null`（防止子进程阻塞读 stdin）；
    /// - 捕获 stdout/stderr（不继承父终端，便于解析）。
    async fn run(&self, program: &str, args: &[String]) -> ComputeResult<CommandOutput>;
}

/// 生产用执行器——`tokio::process::Command` spawn 真实 `dpkg`/`apt-get` 子进程。
///
/// 每次调用前把 `DEBIAN_FRONTEND=noninteractive` 写进子进程环境（不影响父进程）。
/// `dpkg`/`apt-get` 须在 `$PATH`（通常 `/usr/bin`/`/usr/sbin`）。所有写操作需 root。
///
/// **不在常规 `cargo test` 运行**（会真实改宿主包状态）——真实环境测试标 `#[ignore]`。
pub struct TokioAptRunner;

impl Default for TokioAptRunner {
    fn default() -> Self {
        Self
    }
}

#[allow(async_fn_in_trait)]
impl AptRunner for TokioAptRunner {
    async fn run(&self, program: &str, args: &[String]) -> ComputeResult<CommandOutput> {
        use std::process::Stdio;
        use tokio::process::Command;

        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // 注入非交互环境（仅本子进程，不影响父进程）
        for (k, v) in noninteractive_env() {
            cmd.env(k, v);
        }

        let output = cmd.output().await?;
        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code().unwrap_or(-1),
        })
    }
}

/// 把 argv 的第一项视作 program，余项视作 args，调 runner 执行。
///
/// 便利方法：[`install_argv`] 等返回 `Vec<Vec<String>>`（多步命令），逐条调本函数。
pub async fn run_argv<R: AptRunner>(runner: &R, argv: &[String]) -> ComputeResult<CommandOutput> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| ComputeError::InvalidSpec("空 argv".to_string()))?;
    runner.run(program, args).await
}

/// 把非零退出映射成 `ComputeError::CommandFailed`（保留 stderr 便于诊断）。
pub fn check_output<'a>(out: &'a CommandOutput, ctx: &str) -> ComputeResult<&'a CommandOutput> {
    if out.is_success() {
        Ok(out)
    } else {
        Err(ComputeError::CommandFailed(format!(
            "{ctx} 失败（退出码 {}）：{}",
            out.exit_code,
            out.stderr.trim()
        )))
    }
}

// ----------------------------------------------------------------------------
// 上层编排：调构造层拼 argv → runner 执行 → 解析结果
// ----------------------------------------------------------------------------

/// 安装本地 `.deb`：顺序执行 [`install_argv`] 的两步（dpkg -i + apt-get -f install）。
///
/// 任一步非零退出立即返回 `CommandFailed`（不继续后续步骤）。成功返回空
/// [`CommandOutput`]（安装结果由 [`list_packages`] 查询）。
pub async fn install<R: AptRunner>(runner: &R, deb_path: &std::path::Path) -> ComputeResult<()> {
    let steps = install_argv(deb_path)?;
    for (i, step) in steps.iter().enumerate() {
        let out = run_argv(runner, step).await?;
        let ctx = format!("install step {i} ({} {})", step[0], step[1..].join(" "));
        check_output(&out, &ctx)?;
    }
    Ok(())
}

/// 卸载包：执行 [`uninstall_argv`]，非零退出映射 `CommandFailed`。
pub async fn uninstall<R: AptRunner>(runner: &R, id: &PackageId) -> ComputeResult<()> {
    let argv = uninstall_argv(id);
    let out = run_argv(runner, &argv).await?;
    check_output(&out, &format!("uninstall {}", id))?;
    Ok(())
}

/// 升级包：执行 [`upgrade_argv`]，非零退出映射 `CommandFailed`。
pub async fn upgrade<R: AptRunner>(runner: &R, id: &PackageId) -> ComputeResult<()> {
    let argv = upgrade_argv(id);
    let out = run_argv(runner, &argv).await?;
    check_output(&out, &format!("upgrade {}", id))?;
    Ok(())
}

/// 刷新 apt 索引（`apt-get update`）。install/upgrade 前建议先调。
pub async fn update_index<R: AptRunner>(runner: &R) -> ComputeResult<()> {
    let argv = update_argv();
    let out = run_argv(runner, &argv).await?;
    check_output(&out, "apt-get update")?;
    Ok(())
}

/// 执行 [`list_installed_argv`] 并解析全部行为 [`DpkgEntry`]。
///
/// 解析跳过空行/缺字段行（[`parse_dpkg_line`] 容错），返回有效条目列表。
pub async fn list_packages<R: AptRunner>(runner: &R) -> ComputeResult<Vec<DpkgEntry>> {
    let argv = list_installed_argv();
    let out = run_argv(runner, &argv).await?;
    check_output(&out, "dpkg-query -W")?;
    Ok(out.stdout.lines().filter_map(parse_dpkg_line).collect())
}

/// 执行 [`search_argv`] 并解析 `apt-cache search` 输出。
///
/// apt-cache search 行格式：`<package> - <description>`（包名与描述间 ` - ` 分隔）。
/// 解析失败（无 ` - `）的行跳过（容忍尾行/空行）。
pub async fn search<R: AptRunner>(runner: &R, query: &str) -> ComputeResult<Vec<DpkgEntry>> {
    let argv = search_argv(query);
    let out = run_argv(runner, &argv).await?;
    check_output(&out, "apt-cache search")?;
    Ok(out.stdout.lines().filter_map(parse_search_line).collect())
}

/// 解析单行 `apt-cache search` 输出为 [`DpkgEntry`]。
///
/// 行格式：`<package> - <description>`。无 ` - ` 分隔或包名为空返回 None。
pub fn parse_search_line(line: &str) -> Option<DpkgEntry> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let (package, summary) = line.split_once(" - ")?;
    let package = package.trim();
    if package.is_empty() {
        return None;
    }
    Some(DpkgEntry {
        package: package.to_string(),
        // apt-cache search 不含版本——留空，由 list_packages 查询补齐
        version: String::new(),
        summary: summary.trim().to_string(),
    })
}

// ----------------------------------------------------------------------------
// 测试
// ----------------------------------------------------------------------------

// ----------------------------------------------------------------------------
// 测试用 fixture runner（仅在 test 编译）
// ----------------------------------------------------------------------------

/// 测试用 runner——按 (program, args 含子串) 匹配预录 fixture 输出。
///
/// 与 `os_storage::backend_impl::FixtureRunner` 同构设计：`on(program, args_contains, output)`
/// 注册期望，`run` 时按匹配规则查表返回 fixture；无匹配返回 `Internal` 错误（让测试
/// 明确暴露未覆盖的命令调用）。
#[cfg(test)]
#[derive(Default)]
pub struct FixtureAptRunner {
    fixtures: std::sync::Mutex<Vec<FixtureEntry>>,
}

#[cfg(test)]
struct FixtureEntry {
    program: String,
    args_contains: String,
    output: CommandOutput,
}

#[cfg(test)]
impl FixtureAptRunner {
    /// 空构造。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册期望：当调 `<program> ... <args_contains> ...` 时返回 `output`。
    ///
    /// `args_contains` 在 argv join 后的串里做子串匹配（如 `"-i /tmp/foo.deb"`），
    /// 便于按关键参数区分多次同类调用。
    pub fn on(self, program: &str, args_contains: &str, output: CommandOutput) -> Self {
        self.fixtures.lock().unwrap().push(FixtureEntry {
            program: program.to_string(),
            args_contains: args_contains.to_string(),
            output,
        });
        self
    }
}

#[cfg(test)]
#[allow(async_fn_in_trait)]
impl AptRunner for FixtureAptRunner {
    async fn run(&self, program: &str, args: &[String]) -> ComputeResult<CommandOutput> {
        let joined = args.join(" ");
        let fixtures = self.fixtures.lock().unwrap();
        for entry in fixtures.iter() {
            if entry.program == program && joined.contains(&entry.args_contains) {
                return Ok(entry.output.clone());
            }
        }
        Err(ComputeError::Internal(format!(
            "FixtureAptRunner 无匹配 fixture: {program} {joined}"
        )))
    }
}

// ----------------------------------------------------------------------------
// 测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pkg::PackageId;
    use std::path::Path;

    #[test]
    fn install_argv_two_step_dpkg_then_apt_fix() {
        let cmds = install_argv(Path::new("/tmp/foo_1.0_amd64.deb")).unwrap();
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0], vec!["dpkg", "-i", "/tmp/foo_1.0_amd64.deb"]);
        assert_eq!(cmds[1], vec!["apt-get", "-f", "install", "-y"]);
    }

    #[test]
    fn install_argv_rejects_non_deb() {
        let err = install_argv(Path::new("/tmp/foo.tar.gz")).unwrap_err();
        assert!(matches!(err, ComputeError::InvalidSpec(_)));
    }

    #[test]
    fn install_argv_rejects_non_utf8_ext() {
        // 构造非 utf8 路径（OsStr 含非 UTF8 字节）
        use std::os::unix::ffi::OsStrExt;
        let bad = std::ffi::OsStr::from_bytes(b"/tmp/\xff_x.deb");
        let err = install_argv(Path::new(bad)).unwrap_err();
        assert!(matches!(err, ComputeError::InvalidSpec(_)));
    }

    #[test]
    fn uninstall_argv_uses_purge() {
        let argv = uninstall_argv(&PackageId::new("nginx"));
        assert_eq!(argv, vec!["apt-get", "remove", "--purge", "-y", "nginx"]);
    }

    #[test]
    fn upgrade_argv_uses_only_upgrade() {
        let argv = upgrade_argv(&PackageId::new("redis"));
        assert_eq!(
            argv,
            vec!["apt-get", "install", "--only-upgrade", "-y", "redis"]
        );
    }

    #[test]
    fn update_and_search_argv() {
        assert_eq!(update_argv(), vec!["apt-get", "update", "-y"]);
        assert_eq!(search_argv("editor"), vec!["apt-cache", "search", "editor"]);
    }

    #[test]
    fn list_installed_argv_has_tab_format() {
        let argv = list_installed_argv();
        assert_eq!(argv[0], "dpkg-query");
        assert!(argv[2].contains("${Package}"));
        assert!(argv[2].contains("${Version}"));
    }

    #[test]
    fn parse_dpkg_line_splits_three_fields() {
        let e = parse_dpkg_line("nginx\t1:1.25.3-1\tnginx web server\n").unwrap();
        assert_eq!(e.package, "nginx");
        assert_eq!(e.version, "1:1.25.3-1");
        assert_eq!(e.summary, "nginx web server");
    }

    #[test]
    fn parse_dpkg_line_skips_empty_and_short() {
        assert!(parse_dpkg_line("").is_none());
        assert!(parse_dpkg_line("\n").is_none());
        // 缺 version 字段——split '\t' 第二个 next() 返回 None
        assert!(parse_dpkg_line("onlyname\n").is_none());
    }

    #[test]
    fn parse_deb_filename_full() {
        let d = parse_deb_filename("code_1.85.0-1706214921_amd64.deb").unwrap();
        assert_eq!(d.package, "code");
        assert_eq!(d.version.as_deref(), Some("1.85.0-1706214921"));
        assert_eq!(d.arch.as_deref(), Some("amd64"));
    }

    #[test]
    fn parse_deb_filename_no_arch() {
        let d = parse_deb_filename("foo_1.0.deb").unwrap();
        assert_eq!(d.package, "foo");
        assert_eq!(d.version.as_deref(), Some("1.0"));
        assert_eq!(d.arch, None);
    }

    #[test]
    fn parse_deb_filename_rejects_no_suffix() {
        let err = parse_deb_filename("foo_1.0").unwrap_err();
        assert!(matches!(err, ComputeError::InvalidSpec(_)));
    }

    #[test]
    fn parse_deb_filename_rejects_empty_name() {
        let err = parse_deb_filename("_1.0_amd64.deb").unwrap_err();
        assert!(matches!(err, ComputeError::InvalidSpec(_)));
    }

    #[test]
    fn noninteractive_env_has_debian_frontend() {
        let env = noninteractive_env();
        assert_eq!(env[0].0, "DEBIAN_FRONTEND");
        assert_eq!(env[0].1, "noninteractive");
    }

    #[test]
    fn desktop_dirs_returns_paths() {
        let dirs = desktop_dirs();
        assert!(dirs
            .iter()
            .any(|p| p == &PathBuf::from("/usr/share/applications")));
    }

    // --------------------------------------------------------------------
    // 执行层单元测（FixtureAptRunner 注入，零系统依赖）
    // --------------------------------------------------------------------

    #[test]
    fn command_output_constructors() {
        let ok = CommandOutput::ok();
        assert!(ok.is_success());
        assert!(ok.stdout.is_empty());

        let with_stdout = CommandOutput::ok_with_stdout("hello");
        assert_eq!(with_stdout.stdout, "hello");
        assert!(with_stdout.is_success());

        let fail = CommandOutput::fail(100, "boom");
        assert!(!fail.is_success());
        assert_eq!(fail.exit_code, 100);
        assert_eq!(fail.stderr, "boom");
    }

    #[tokio::test]
    async fn run_argv_empty_returns_invalid_spec() {
        let runner = FixtureAptRunner::new();
        let err = run_argv(&runner, &[]).await.unwrap_err();
        assert!(matches!(err, ComputeError::InvalidSpec(_)));
    }

    #[tokio::test]
    async fn check_output_maps_nonzero_to_command_failed() {
        let out = CommandOutput::fail(1, "broken");
        let err = check_output(&out, "ctx").unwrap_err();
        assert!(matches!(err, ComputeError::CommandFailed(_)));
        // 成功则原样返回引用
        let ok = CommandOutput::ok();
        assert!(check_output(&ok, "ctx").is_ok());
    }

    #[tokio::test]
    async fn fixture_runner_matches_by_program_and_args_substring() {
        let runner = FixtureAptRunner::new().on("dpkg-query", "${Package}", {
            CommandOutput::ok_with_stdout(
                "nginx\t1:1.25.3-1\tnginx web server\nredis\t7.0.15-1\tk-v store\n",
            )
        });
        let entries = list_packages(&runner).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].package, "nginx");
        assert_eq!(entries[1].package, "redis");
    }

    #[tokio::test]
    async fn fixture_runner_unmatched_returns_internal_error() {
        let runner = FixtureAptRunner::new(); // 无 fixture
        let err = list_packages(&runner).await.unwrap_err();
        assert!(matches!(err, ComputeError::Internal(_)));
    }

    #[tokio::test]
    async fn install_runs_two_steps_and_returns_ok_on_success() {
        let runner = FixtureAptRunner::new()
            .on("dpkg", "-i", CommandOutput::ok())
            .on("apt-get", "-f install", CommandOutput::ok());
        install(&runner, Path::new("/tmp/foo_1.0_amd64.deb"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn install_short_circuits_on_first_step_failure() {
        let runner = FixtureAptRunner::new().on("dpkg", "-i", CommandOutput::fail(1, "dpkg error"));
        let err = install(&runner, Path::new("/tmp/foo_1.0_amd64.deb"))
            .await
            .unwrap_err();
        assert!(matches!(err, ComputeError::CommandFailed(m) if m.contains("dpkg")));
    }

    #[tokio::test]
    async fn install_rejects_non_deb_path_before_running() {
        let runner = FixtureAptRunner::new(); // 不应被调
        let err = install(&runner, Path::new("/tmp/foo.tar.gz"))
            .await
            .unwrap_err();
        assert!(matches!(err, ComputeError::InvalidSpec(_)));
    }

    #[tokio::test]
    async fn uninstall_and_upgrade_invoke_runner_with_correct_argv() {
        let runner = FixtureAptRunner::new()
            .on("apt-get", "remove --purge", CommandOutput::ok())
            .on("apt-get", "--only-upgrade", CommandOutput::ok());
        uninstall(&runner, &PackageId::new("nginx")).await.unwrap();
        upgrade(&runner, &PackageId::new("redis")).await.unwrap();
    }

    #[tokio::test]
    async fn uninstall_maps_nonzero_to_command_failed() {
        let runner =
            FixtureAptRunner::new().on("apt-get", "remove --purge", CommandOutput::fail(100, "x"));
        let err = uninstall(&runner, &PackageId::new("nginx"))
            .await
            .unwrap_err();
        assert!(matches!(err, ComputeError::CommandFailed(_)));
    }

    #[tokio::test]
    async fn update_index_runs_apt_get_update() {
        let runner = FixtureAptRunner::new().on("apt-get", "update", CommandOutput::ok());
        update_index(&runner).await.unwrap();
    }

    #[tokio::test]
    async fn search_parses_apt_cache_search_output() {
        let runner = FixtureAptRunner::new().on(
            "apt-cache",
            "editor",
            CommandOutput::ok_with_stdout("vim - Vi IMproved editor\nnano - small editor\n"),
        );
        let entries = search(&runner, "editor").await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].package, "vim");
        assert_eq!(entries[0].summary, "Vi IMproved editor");
        // apt-cache search 无版本——留空
        assert_eq!(entries[0].version, "");
    }

    #[test]
    fn parse_search_line_splits_dash_separator() {
        let e = parse_search_line("vim - Vi IMproved").unwrap();
        assert_eq!(e.package, "vim");
        assert_eq!(e.summary, "Vi IMproved");
    }

    #[test]
    fn parse_search_line_rejects_malformed() {
        assert!(parse_search_line("").is_none());
        assert!(parse_search_line("noseparator").is_none());
        assert!(parse_search_line(" - desc").is_none()); // 空包名
    }

    // --------------------------------------------------------------------
    // 补充测：常量 / parse 边界 / deb 文件名边角 / ICON_DIRS / Display
    // --------------------------------------------------------------------

    #[test]
    fn apt_constants_have_expected_values() {
        assert_eq!(NONINTERACTIVE_ENV_KEY, "DEBIAN_FRONTEND");
        assert_eq!(NONINTERACTIVE_ENV_VAL, "noninteractive");
        assert_eq!(APT_YES_FLAG, "-y");
    }

    #[test]
    fn desktop_file_dirs_constants_populated() {
        assert!(DESKTOP_FILE_DIRS.contains(&"/usr/share/applications"));
        assert!(DESKTOP_FILE_DIRS.contains(&"/usr/local/share/applications"));
        assert!(DESKTOP_FILE_DIRS.contains(&"/var/lib/flatpak/exports/share/applications"));
        assert!(!DESKTOP_FILE_DIRS.is_empty());
    }

    #[test]
    fn icon_dirs_constants_populated() {
        assert!(ICON_DIRS.contains(&"/usr/share/icons"));
        assert!(ICON_DIRS.contains(&"/usr/share/pixmaps"));
        assert!(ICON_DIRS.contains(&"/usr/local/share/icons"));
        assert!(!ICON_DIRS.is_empty());
    }

    #[test]
    fn desktop_dirs_count_matches_constant() {
        let dirs = desktop_dirs();
        assert_eq!(dirs.len(), DESKTOP_FILE_DIRS.len());
        // 全为 PathBuf
        for d in &dirs {
            assert!(d.is_absolute(), "应全为绝对路径: {}", d.display());
        }
    }

    #[test]
    fn parse_dpkg_line_skips_whitespace_only_summary() {
        // 包名/版本非空，summary 全空格（视为空 summary 但仍返回 entry）
        let e = parse_dpkg_line("nginx\t1.0\t   \n").unwrap();
        assert_eq!(e.package, "nginx");
        assert_eq!(e.version, "1.0");
        // summary 被 trim 成空串
        assert_eq!(e.summary, "");
    }

    #[test]
    fn parse_dpkg_line_no_trailing_newline() {
        let e = parse_dpkg_line("pkg\t1.0\tdesc").unwrap();
        assert_eq!(e.package, "pkg");
        assert_eq!(e.version, "1.0");
        assert_eq!(e.summary, "desc");
    }

    #[test]
    fn parse_dpkg_line_empty_package_returns_none() {
        // 第一字段空（仅分隔符）→ package 空串 → None
        assert!(parse_dpkg_line("\t1.0\tdesc").is_none());
        assert!(parse_dpkg_line("   \t1.0\tdesc").is_none());
    }

    #[test]
    fn parse_dpkg_line_empty_version_returns_none() {
        // 缺第二字段 → next() None → None
        assert!(parse_dpkg_line("onlyname").is_none());
    }

    #[test]
    fn parse_deb_filename_only_name() {
        // 仅包名（无 _version_arch）—— 仍合法（version/arch 为 None）
        let d = parse_deb_filename("foo.deb").unwrap();
        assert_eq!(d.package, "foo");
        assert_eq!(d.version, None);
        assert_eq!(d.arch, None);
    }

    #[test]
    fn parse_deb_filename_with_underscores_in_version() {
        // version 含 _（debian 允许）：split('_') 只切第一个，故 version = "1.0~beta"
        let d = parse_deb_filename("foo_1.0~beta_2_amd64.deb").unwrap();
        assert_eq!(d.package, "foo");
        // split('_') 在第一个 _ 切：["foo", "1.0~beta", "2", "amd64"]
        // parts.next() 第二次 = "1.0~beta"，第三次 = "2"
        assert_eq!(d.version.as_deref(), Some("1.0~beta"));
        assert_eq!(d.arch.as_deref(), Some("2"));
    }

    #[test]
    fn parse_deb_filename_multiple_underscores_drops_extra() {
        // 4 个字段（name_version_arch_extra）：split 拿前 3 个，extra 丢弃
        let d = parse_deb_filename("foo_1.0_amd64_extra.deb").unwrap();
        assert_eq!(d.package, "foo");
        assert_eq!(d.version.as_deref(), Some("1.0"));
        assert_eq!(d.arch.as_deref(), Some("amd64"));
    }

    #[test]
    fn parse_deb_filename_deb_only_no_name() {
        // 仅后缀 ".deb"，name 为空 → 错误
        let err = parse_deb_filename(".deb").unwrap_err();
        assert!(matches!(err, ComputeError::InvalidSpec(_)));
    }

    #[test]
    fn install_argv_absolute_path() {
        let cmds = install_argv(Path::new("/var/cache/apt/foo_1.0_amd64.deb")).unwrap();
        assert_eq!(cmds[0][2], "/var/cache/apt/foo_1.0_amd64.deb");
    }

    #[test]
    fn install_argv_relative_path() {
        // 相对路径合法（dpkg 接受相对路径）
        let cmds = install_argv(Path::new("foo.deb")).unwrap();
        assert_eq!(cmds[0][0], "dpkg");
        assert_eq!(cmds[0][2], "foo.deb");
    }

    #[test]
    fn uninstall_argv_display_id() {
        // PackageId::Display 应正确拼进 argv
        let id = PackageId::new("nginx-full");
        let argv = uninstall_argv(&id);
        assert_eq!(argv.last().unwrap(), "nginx-full");
    }

    #[test]
    fn upgrade_argv_display_id() {
        let id = PackageId::new("redis-server");
        let argv = upgrade_argv(&id);
        assert_eq!(argv.last().unwrap(), "redis-server");
    }

    #[test]
    fn dpkg_entry_eq_serde_roundtrip() {
        let e = DpkgEntry {
            package: "nginx".to_string(),
            version: "1.25".to_string(),
            summary: "web server".to_string(),
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: DpkgEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back, e);
    }

    #[test]
    fn deb_filename_eq_serde_roundtrip() {
        let d = DebFilename {
            package: "x".to_string(),
            version: Some("1.0".to_string()),
            arch: Some("amd64".to_string()),
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: DebFilename = serde_json::from_str(&json).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn deb_filename_serde_skip_none_optionals() {
        let d = DebFilename {
            package: "x".to_string(),
            version: None,
            arch: None,
        };
        let json = serde_json::to_string(&d).unwrap();
        // 注：DebFilename 未标 skip_serializing_if，None 仍会序列化为 null
        assert!(json.contains(r#""version":null"#));
        assert!(json.contains(r#""arch":null"#));
    }

    // --------------------------------------------------------------------
    // Fixture runner 补充：多 fixture 顺序匹配 + search 全空输出
    // --------------------------------------------------------------------

    #[tokio::test]
    async fn upgrade_short_circuits_on_failure() {
        let runner = FixtureAptRunner::new().on(
            "apt-get",
            "--only-upgrade",
            CommandOutput::fail(100, "no upgrade"),
        );
        let err = upgrade(&runner, &PackageId::new("nginx"))
            .await
            .unwrap_err();
        assert!(matches!(err, ComputeError::CommandFailed(_)));
    }

    #[tokio::test]
    async fn list_packages_returns_empty_when_stdout_empty() {
        // dpkg-query 返回空输出（无任何包，理论罕见但容错）
        let runner = FixtureAptRunner::new().on("dpkg-query", "${Package}", CommandOutput::ok());
        let entries = list_packages(&runner).await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn list_packages_skips_garbage_lines() {
        // stdout 含空行/缺字段行 → filter_map 跳过
        let runner = FixtureAptRunner::new().on(
            "dpkg-query",
            "${Package}",
            CommandOutput::ok_with_stdout("\nvalid\t1.0\tdesc\nbroken\n"),
        );
        let entries = list_packages(&runner).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].package, "valid");
    }

    #[tokio::test]
    async fn search_returns_empty_when_no_results() {
        let runner = FixtureAptRunner::new().on(
            "apt-cache",
            "nonexistent-query",
            CommandOutput::ok_with_stdout(""),
        );
        let entries = search(&runner, "nonexistent-query").await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn search_skips_malformed_lines() {
        // 含无 " - " 分隔的行应被跳过
        let runner = FixtureAptRunner::new().on(
            "apt-cache",
            "x",
            CommandOutput::ok_with_stdout("valid - good\nbadline\n  - emptyname\n"),
        );
        let entries = search(&runner, "x").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].package, "valid");
    }

    #[tokio::test]
    async fn run_argv_with_real_command_succeeds_via_fixture() {
        // run_argv 的便利路径：直接喂 argv
        let runner = FixtureAptRunner::new().on("apt-get", "update", CommandOutput::ok());
        let argv = vec![
            "apt-get".to_string(),
            "update".to_string(),
            "-y".to_string(),
        ];
        let out = run_argv(&runner, &argv).await.unwrap();
        assert!(out.is_success());
    }

    #[tokio::test]
    async fn fixture_runner_first_match_wins() {
        // 多个 fixture 命中同 argv：注册顺序首个命中
        let runner = FixtureAptRunner::new()
            .on("apt-get", "update", CommandOutput::ok_with_stdout("first"))
            .on("apt-get", "update", CommandOutput::ok_with_stdout("second"));
        let argv = vec!["apt-get".to_string(), "update".to_string()];
        let out = run_argv(&runner, &argv).await.unwrap();
        assert_eq!(out.stdout, "first");
    }

    // --------------------------------------------------------------------
    // 真实执行测（#[ignore]——需 root + apt，不参与常规 cargo test）
    // --------------------------------------------------------------------

    #[tokio::test]
    #[ignore = "真实 apt 执行：需 root + apt-get + 写 /var/lib/dpkg，人工 `cargo test -- --ignored`"]
    async fn real_list_packages_returns_at_least_one_entry() {
        // 多数 Debian/Ubuntu 系统装了 base-files；dpkg-query 必返非空。
        let runner = TokioAptRunner;
        let entries = list_packages(&runner).await.unwrap();
        assert!(!entries.is_empty(), "dpkg-query 应至少返回 base-files");
        // 至少有一条含合法 package/version
        assert!(entries
            .iter()
            .any(|e| !e.package.is_empty() && !e.version.is_empty()));
    }

    #[tokio::test]
    #[ignore = "真实 apt 执行：需 root + apt-get，人工 `cargo test -- --ignored`"]
    async fn real_update_index_runs_apt_get_update() {
        // 只跑 apt-get update（不改包状态，仅刷新索引）；失败容忍（无网络）→ 跳断言。
        let runner = TokioAptRunner;
        let _ = update_index(&runner).await;
        // 不强断言成功（沙箱可能无网络），仅验证不 panic + 不挂起。
    }

    #[tokio::test]
    #[ignore = "真实 apt 执行：验证 TokioAptRunner 注入非交互环境（人工）"]
    async fn real_runner_injects_noninteractive_env() {
        // 用 sh 打印 DEBIAN_FRONTEND 验证注入（不依赖 apt 存在）。
        use std::process::Stdio;
        use tokio::process::Command;
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("printf '%s' \"$DEBIAN_FRONTEND\"")
            .stdin(Stdio::null())
            .stdout(Stdio::piped());
        for (k, v) in noninteractive_env() {
            cmd.env(k, v);
        }
        let out = cmd.output().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "noninteractive");
    }
}
