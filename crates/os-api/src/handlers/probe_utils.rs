//! 外部工具探测工具集（v0.1.51 三引擎剥离批下沉）。
//!
//! film.rs / film_hub/blender.rs 里的通用探测函数（ffmpeg / blender 的
//! env→PATH→常规路径三态解析链 + 路径可执行判定）在引擎剥离
//! （film / streaming / surveillance 删除出 os-api，独立产品线
//! FilmStudio/StreamingStudio 承接）后仍有系统内消费者：
//!
//! - [`is_executable`]：monitor.rs 显存采集（vraminfo 解析链末端过滤）；
//! - [`detect_ffmpeg`]：capabilities.rs 能力快照 `media.ffmpeg_available`
//!   （媒体生成 media_gen 的内核描述仍以 ffmpeg 可用性为能力位）。
//!
//! 函数体零改只搬家（自 film.rs / film_hub/blender.rs 原样迁入）；
//! [`detect_blender`] 当前无系统内消费者（vraminfo 自带独立探测链，
//! 不经此处），按剥离批冻结清单一并下沉保留（#[allow(dead_code)]——
//! 独立产品线参考实现 / 未来资产管线复用）。

// ----------------------------------------------------------------------------
// 路径可执行判定
// ----------------------------------------------------------------------------

/// 路径可执行探测（文件 + 任一 x 位；llm_envs is_executable 同款）。
pub(crate) fn is_executable(path: &str) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && (m.permissions().mode() & 0o111) != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        std::fs::metadata(path)
            .map(|m| m.is_file())
            .unwrap_or(false)
    }
}

// ----------------------------------------------------------------------------
// ffmpeg 探测（自 film.rs 零改迁入）
// ----------------------------------------------------------------------------

/// ffmpeg 常规落点（PATH 扫描之外的兜底候选，按序探测）。
const FFMPEG_COMMON_PATHS: [&str; 5] = [
    "/usr/bin/ffmpeg",
    "/usr/local/bin/ffmpeg",
    "/bin/ffmpeg",
    "/opt/homebrew/bin/ffmpeg",
    "/snap/bin/ffmpeg",
];

/// ffmpeg 解析内核（参数化，测试注入合成值，不读进程 env）：
/// env 覆写（可执行才认）→ PATH 目录扫描 → 常规路径候选。
#[must_use]
pub fn detect_ffmpeg_with(
    env_bin: Option<&str>,
    path_dirs: &[String],
    extra_candidates: &[&str],
) -> Option<String> {
    if let Some(b) = env_bin.map(str::trim).filter(|s| !s.is_empty()) {
        if is_executable(b) {
            return Some(b.to_string());
        }
    }
    for d in path_dirs {
        let cand = if d.ends_with('/') {
            format!("{d}ffmpeg")
        } else {
            format!("{d}/ffmpeg")
        };
        if is_executable(&cand) {
            return Some(cand);
        }
    }
    extra_candidates
        .iter()
        .find(|p| is_executable(p))
        .map(|p| (*p).to_string())
}

/// 请求路径的 ffmpeg 解析链（env `NEXOS_FFMPEG_BIN` → PATH → 常规路径）。
#[must_use]
pub fn detect_ffmpeg() -> Option<String> {
    let path_dirs: Vec<String> = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|d| !d.is_empty())
        .map(String::from)
        .collect();
    detect_ffmpeg_with(
        std::env::var("NEXOS_FFMPEG_BIN").ok().as_deref(),
        &path_dirs,
        &FFMPEG_COMMON_PATHS,
    )
}

// ----------------------------------------------------------------------------
// blender 探测（自 film_hub/blender.rs 零改迁入）
// ----------------------------------------------------------------------------

/// blender 常规落点（PATH 扫描之外的兜底候选，按序探测）。
const BLENDER_COMMON_PATHS: [&str; 5] = [
    "/usr/bin/blender",
    "/usr/local/bin/blender",
    "/bin/blender",
    "/snap/bin/blender",
    "/opt/blender/blender",
];

/// blender 解析内核（参数化，测试注入合成值，不读进程 env；同 ffmpeg 口径）：
/// env 覆写（可执行才认）→ PATH 目录扫描 → 常规路径候选。
#[must_use]
pub fn detect_blender_with(
    env_bin: Option<&str>,
    path_dirs: &[String],
    extra_candidates: &[&str],
) -> Option<String> {
    if let Some(b) = env_bin.map(str::trim).filter(|s| !s.is_empty()) {
        if is_executable(b) {
            return Some(b.to_string());
        }
    }
    for d in path_dirs {
        let cand = if d.ends_with('/') {
            format!("{d}blender")
        } else {
            format!("{d}/blender")
        };
        if is_executable(&cand) {
            return Some(cand);
        }
    }
    extra_candidates
        .iter()
        .find(|p| is_executable(p))
        .map(|p| (*p).to_string())
}

/// 请求路径的 blender 解析链（env `NEXOS_BLENDER_BIN` → PATH → 常规路径）。
///
/// 引擎剥离后暂无系统内消费者（见模块头），保留供独立产品线参考。
#[allow(dead_code)]
#[must_use]
pub fn detect_blender() -> Option<String> {
    let path_dirs: Vec<String> = std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .filter(|d| !d.is_empty())
        .map(String::from)
        .collect();
    detect_blender_with(
        std::env::var("NEXOS_BLENDER_BIN").ok().as_deref(),
        &path_dirs,
        &BLENDER_COMMON_PATHS,
    )
}

// ----------------------------------------------------------------------------
// 测试（自 film.rs / film_hub/blender.rs 搬家保留，语义零改）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir_for(test: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("nexos-probe-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fake_exec(dir: &std::path::Path, name: &str, content: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        let mut perm = std::fs::metadata(&path).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&path, perm).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn detect_ffmpeg_finds_env_path_and_candidates() {
        let dir = temp_dir_for("ffmpeg-detect");
        let bin = fake_exec(&dir, "ffmpeg", "#!/bin/sh\nexit 0\n");
        // env 注入（可执行）优先
        assert_eq!(
            detect_ffmpeg_with(Some(bin.to_str().unwrap()), &[], &[]),
            Some(bin.to_string_lossy().into_owned())
        );
        // env 指向不存在文件 → 跳过 → PATH 目录扫描
        assert_eq!(
            detect_ffmpeg_with(
                Some("/nonexistent/ffmpeg"),
                &[dir.to_string_lossy().into_owned()],
                &[]
            ),
            Some(bin.to_string_lossy().into_owned()),
            "PATH 扫描应命中"
        );
        // 兜底候选位
        assert_eq!(
            detect_ffmpeg_with(None, &[], &[bin.to_str().unwrap()]),
            Some(bin.to_string_lossy().into_owned())
        );
        // 全空 → None（缺失即报安装指引，不猜）
        assert_eq!(detect_ffmpeg_with(None, &[], &[]), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn detect_blender_finds_env_path_and_candidates() {
        let dir = temp_dir_for("blender-detect");
        let bin = fake_exec(&dir, "blender", "#!/bin/sh\nexit 0\n");
        assert_eq!(
            detect_blender_with(Some(bin.to_str().unwrap()), &[], &[]),
            Some(bin.to_string_lossy().into_owned())
        );
        assert_eq!(
            detect_blender_with(
                Some("/nonexistent/blender"),
                &[dir.to_string_lossy().into_owned()],
                &[]
            ),
            Some(bin.to_string_lossy().into_owned()),
            "PATH 扫描应命中"
        );
        assert_eq!(
            detect_blender_with(None, &[], &[bin.to_str().unwrap()]),
            Some(bin.to_string_lossy().into_owned())
        );
        assert_eq!(detect_blender_with(None, &[], &[]), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_executable_rejects_missing_and_non_exec() {
        assert!(!is_executable("/nonexistent/definitely-missing"));
        let dir = temp_dir_for("is-exec");
        let plain = dir.join("plain.txt");
        std::fs::write(&plain, "x").unwrap();
        assert!(!is_executable(plain.to_str().unwrap()), "无 x 位不可执行");
        #[cfg(unix)]
        {
            let exec = fake_exec(&dir, "run.sh", "#!/bin/sh\n");
            assert!(is_executable(exec.to_str().unwrap()));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 请求路径包装（读真实 env/PATH）：合法运行环境本身必须有 /bin 或
    /// /usr/bin；只断言「不 panic + 返回 Option」形态，不断言具体路径。
    #[test]
    fn detect_ffmpeg_request_path_well_formed() {
        let _ = detect_ffmpeg();
        let _ = detect_blender();
    }
}
