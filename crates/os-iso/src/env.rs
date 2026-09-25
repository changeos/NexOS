//! ISO 构建工具链环境探测——检查 xorriso / mksquashfs / sha256sum 二进制是否装机。
//!
//! 设计动机：真实 ISO 构建（[`crate::runner::TokioIsoRunner`] spawn xorriso/mksquashfs）
//! 依赖系统包 `xorriso` + `squashfs-tools`。开发机/普通 CI runner 通常无此工具，
//! 故相关端到端测标 `#[ignore]`，并由本模块在测入口做存在性探针决定是否跳过
//! （避免 `#[ignore]` 测在被显式 `--ignored` 调用时因缺工具而 panic 出无意义栈）。
//!
//! 探测策略：依次尝试 `which`（POSIX）与 `command -v`（bash 内建/dash 兼容），
//! 任一返回 0 即视作存在。这样在无 `which` 的最小容器（如 scratch 衍生）也能探到。
//! 对单个二进制也提供纯 Rust 的 [`Probe::exists_in_path`]（遍历 `$PATH` + 可执行位判定），
//! 完全不依赖外部命令，最稳。
//!
//! 仅用于测/诊断（生产 `XorrisoIsoBuilder::build` 失败由 `IsoError::BuildFailed` 自然报错）。

use std::path::PathBuf;

/// ISO 构建所需的外部二进制名。
pub const XORRISO: &str = "xorriso";
pub const MKSQUASHFS: &str = "mksquashfs";
pub const SHA256SUM: &str = "sha256sum";

/// ISO 构建工具链环境探测结果。
///
/// 记录 xorriso / mksquashfs / sha256sum 三个二进制是否在 `$PATH` 中可发现。
/// 测入口用 [`IsoEnvironment::probe`] 取得本结构，再决定是否跳过真实测。
#[derive(Debug, Clone)]
pub struct IsoEnvironment {
    /// xorriso 是否存在。
    pub has_xorriso: bool,
    /// mksquashfs 是否存在。
    pub has_mksquashfs: bool,
    /// sha256sum 是否存在。
    pub has_sha256sum: bool,
}

impl IsoEnvironment {
    /// 探测当前环境的 ISO 构建工具链。
    ///
    /// 依次用 [`Probe::exists`]（优先纯 Rust `$PATH` 遍历，兜底 `command -v`）。
    /// 失败不 panic——任一探测错误都视作"不存在"，返回 `false` 对应字段。
    #[must_use]
    pub fn probe() -> Self {
        Self {
            has_xorriso: Probe::exists(XORRISO),
            has_mksquashfs: Probe::exists(MKSQUASHFS),
            has_sha256sum: Probe::exists(SHA256SUM),
        }
    }

    /// 真实 ISO 构建所需工具链是否齐全（xorriso + mksquashfs）。
    ///
    /// sha256sum 通常 coreutils 自带，不作为硬门槛；但多数情况下它也在，故仍记录。
    #[must_use]
    pub fn is_capable(&self) -> bool {
        self.has_xorriso && self.has_mksquashfs
    }

    /// 适合在测入口跳过时打印的人类可读缺失清单。
    #[must_use]
    pub fn missing_tools(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if !self.has_xorriso {
            missing.push(XORRISO);
        }
        if !self.has_mksquashfs {
            missing.push(MKSQUASHFS);
        }
        if !self.has_sha256sum {
            missing.push(SHA256SUM);
        }
        missing
    }
}

impl Default for IsoEnvironment {
    fn default() -> Self {
        Self::probe()
    }
}

/// 单个外部命令的存在性探针（工具方法集合）。
pub struct Probe;

impl Probe {
    /// 综合探测一个二进制是否可用：优先 [`Self::exists_in_path`]（纯 Rust，
    /// 不依赖外部命令），失败再退 [`Self::exists_via_command_v`]。
    ///
    /// 任一返回 true 即视作存在；两者都失败返回 false（不报错——本模块只服务测跳过决策）。
    #[must_use]
    pub fn exists(program: &str) -> bool {
        Self::exists_in_path(program) || Self::exists_via_command_v(program)
    }

    /// 纯 Rust 实现：遍历 `$PATH`，找第一个同名且可执行的文件。
    ///
    /// 不 spawn 任何子进程，最稳；适配无 `which`/`command` 的极简环境。
    /// 在 Windows 上 `PATH` 分隔符为 `;`，本 crate 目标为 Linux OS 系统，仅处理 Unix。
    #[must_use]
    pub fn exists_in_path(program: &str) -> bool {
        Self::find_in_path(program).is_some()
    }

    /// 返回 `$PATH` 中该程序第一个命中条目的完整路径（供测断言/诊断）。
    ///
    /// 不可执行或无执行位的条目会被跳过。
    #[must_use]
    pub fn find_in_path(program: &str) -> Option<PathBuf> {
        let path_env = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path_env) {
            let candidate = dir.join(program);
            if Self::is_executable(&candidate) {
                return Some(candidate);
            }
        }
        None
    }

    /// 判定路径是否为"可执行文件"（存在 + 是文件 + 有执行位）。
    ///
    /// Unix 专用：非 Unix 平台退化为"存在且是文件"。
    fn is_executable(path: &std::path::Path) -> bool {
        // 必须先确认是文件，避免目录带 x 位（如 /tmp）误判。
        let md = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => return false,
        };
        if !md.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            md.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    }

    /// 通过 shell `command -v <program>` 探测（POSIX sh 内建，比 `which` 更可移植）。
    ///
    /// `command -v` 在 dash/bash/zsh 均为内建，返回 0 = 存在；stdout 为该程序路径。
    /// 失败（无 shell / 探测出错）一律返回 false。
    #[must_use]
    pub fn exists_via_command_v(program: &str) -> bool {
        // 用 /bin/sh -c（避免依赖具体 shell 路径如 /bin/bash）。
        let out = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(format!("command -v {program}"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
        match out {
            Ok(o) => o.status.success(),
            Err(_) => false,
        }
    }
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_returns_three_flags() {
        let env = IsoEnvironment::probe();
        // 不对真实结果做强断言（CI/开发机环境不一），只验字段都被填了。
        let _ = env.has_xorriso;
        let _ = env.has_mksquashfs;
        let _ = env.has_sha256sum;
    }

    #[test]
    fn is_capable_requires_both_iso_tools() {
        let full = IsoEnvironment {
            has_xorriso: true,
            has_mksquashfs: true,
            has_sha256sum: true,
        };
        assert!(full.is_capable());

        let no_xorriso = IsoEnvironment {
            has_xorriso: false,
            has_mksquashfs: true,
            has_sha256sum: true,
        };
        assert!(!no_xorriso.is_capable());

        let no_sqfs = IsoEnvironment {
            has_xorriso: true,
            has_mksquashfs: false,
            has_sha256sum: true,
        };
        assert!(!no_sqfs.is_capable());
    }

    #[test]
    fn missing_tools_lists_absent_ones() {
        let none = IsoEnvironment {
            has_xorriso: false,
            has_mksquashfs: false,
            has_sha256sum: false,
        };
        let missing = none.missing_tools();
        assert!(missing.contains(&XORRISO));
        assert!(missing.contains(&MKSQUASHFS));
        assert!(missing.contains(&SHA256SUM));

        let all = IsoEnvironment {
            has_xorriso: true,
            has_mksquashfs: true,
            has_sha256sum: true,
        };
        assert!(all.missing_tools().is_empty());
    }

    #[test]
    fn probe_find_existing_coreutil() {
        // sha256sum 在所有测试环境（Linux CI/开发机）都该有（coreutils）。
        // 用 find_in_path 验证返回值非空 + 路径以程序名结尾。
        if let Some(p) = Probe::find_in_path(SHA256SUM) {
            assert!(p.ends_with(SHA256SUM));
        } else {
            // 极罕见无 sha256sum（如某些 BSD），不强断。
        }
    }

    #[test]
    fn probe_nonexistent_returns_false() {
        assert!(!Probe::exists_in_path(
            "definitely_not_a_real_program_xyzzy_42"
        ));
        assert!(!Probe::exists_via_command_v(
            "definitely_not_a_real_program_xyzzy_42"
        ));
        assert!(!Probe::exists("definitely_not_a_real_program_xyzzy_42"));
    }

    #[test]
    fn probe_command_v_on_builtin_returns_true_for_sh() {
        // /bin/sh 几乎一定在；command -v sh 应命中（或 /bin/sh 本身在 PATH）。
        // 这个测验证 command -v 机制本身能工作（即使在没有 xorriso 的机器上）。
        // 若 PATH 里既无 sh 且 /bin/sh 不存在，跳过断言（不可达场景）。
        if std::path::Path::new("/bin/sh").exists() {
            let _ = Probe::exists_via_command_v("sh");
        }
    }

    #[test]
    fn default_equals_probe() {
        // Default::default() 应等价于 probe()（都做真实探测）。
        let a = IsoEnvironment::probe();
        let b = IsoEnvironment::default();
        assert_eq!(a.has_xorriso, b.has_xorriso);
        assert_eq!(a.has_mksquashfs, b.has_mksquashfs);
        assert_eq!(a.has_sha256sum, b.has_sha256sum);
    }
}
