//! FFmpeg HLS 转码编排（外部二进制子进程）。
//!
//! **定位**：本模块是「编排层」，不是 FFmpeg 绑定——FFmpeg 是外部二进制
//! （不在 workspace 注册 Rust crate），由 [`TokioFfmpegRunner`] 用
//! `tokio::process::Command` spawn 真实子进程。命令构造（[build_hls_args`] /
//! [`build_master_playlist`]/[`build_media_playlist`])是**纯函数**，可独立测试；
//! 子进程执行经 [`FfmpegRunner`] trait 抽象，测试注入 fixture runner
//! （[`FixtureFfmpegRunner`]）——`cargo test` 无需真实 FFmpeg。
//!
//! 参考约定：与 `os-storage::backend_impl::CommandRunner` 同构（外部 CLI 编排
//! 的统一模式），保持代码风格一致。
//!
//! **能力**：
//! - [`TranscodeProfile`] → FFmpeg 参数映射（[build_hls_args`]）。
//! - 单档位 HLS 转码命令构造：`ffmpeg -i <src> -vf scale=-2:<h> -c:v libx264
//!   -c:a aac -hls_time 6 -hls_playlist_type vod -f hls <out>.m3u8`。
//! - 自适应码率（ABR）多 profile：并行转出多个变体 → [`build_master_playlist`]
//!   产出 master.m3u8。
//! - HLS media playlist 生成（[build_media_playlist`]），用于不真跑 FFmpeg 时
//!   的产物构造。
//!
//! **未接入 / TODO** \[RUNTIME\]（运行时硬阻塞，依赖未注册/外部二进制）：
//! - 真实 FFmpeg 二进制路径（默认 `ffmpeg`，可经 [`FfmpegRunner`] 注入）。
//! - HLS 加密（key file + IV）—— 留接口点（[HlsVariant::key_file`]）。
//!
//! 参考：Immich / Apple HLS Authoring Specification。

use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::Command;

use crate::media::TranscodeProfile;
use crate::ServiceError;

// ----------------------------------------------------------------------------
// 命令执行抽象
// ----------------------------------------------------------------------------

/// FFmpeg 子进程执行结果。
///
/// **统一来源**（review2 P-R2-1）：原 `media_ffmpeg` 模块独立定义的 `FfmpegOutput`
/// 与 `os_storage::backend_impl::CommandOutput` / `os-compute` 同构。现统一到
/// [`os_core::CommandOutput`]（字段 `stdout/stderr/exit_code`，构造器 `ok/ok_with_stdout/
/// fail/is_success`）。
///
/// 本 crate 保留名称 `FfmpegOutput` 作为类型别名，避免改动 [`FfmpegRunner::run`] 的
/// trait 签名（P-R2-1 红线：不改 trait 签名）。别名即 `os_core::CommandOutput`，
/// 所有字段访问与构造器（[`FfmpegOutput::ok`] 等）行为完全一致。
pub type FfmpegOutput = os_core::CommandOutput;

/// FFmpeg 命令执行器抽象——隔离子进程 spawn，使编排逻辑可测。
///
/// 生产用 [`TokioFfmpegRunner`]（spawn 真实 `ffmpeg`）；测试用
/// [`FixtureFfmpegRunner`] 注入确定输出。命令构造（[build_hls_args`]）
/// 在调用方完成，本 trait 仅负责 spawn + 收集 output。
#[async_trait]
pub trait FfmpegRunner: Send + Sync {
    /// 执行 `ffmpeg <args...>`，等待完成并收集 stdout/stderr/退出码。
    async fn run(&self, args: &[String]) -> Result<FfmpegOutput, ServiceError>;
}

/// 生产用执行器——`tokio::process::Command` spawn 真实 `ffmpeg` 子进程。
///
/// `ffmpeg` 必须在 `$PATH`（或经自定义构造器指定路径）。stderr 捕获并保留。
/// 超时与进度回调留 TODO \[RUNTIME\]（运行时编排增强，需真实 ffmpeg 子进程）。
pub struct TokioFfmpegRunner {
    /// FFmpeg 二进制路径（默认 `ffmpeg`）。
    binary: String,
}

impl TokioFfmpegRunner {
    /// 构造默认执行器（`binary = "ffmpeg"`）。
    pub fn new() -> Self {
        Self {
            binary: "ffmpeg".to_string(),
        }
    }

    /// 指定 FFmpeg 二进制路径（生产部署 ffmpeg 不在 `$PATH` 时用）。
    pub fn with_binary(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
        }
    }

    /// 取二进制路径（测试观测用）。
    pub fn binary(&self) -> &str {
        &self.binary
    }
}

impl Default for TokioFfmpegRunner {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl FfmpegRunner for TokioFfmpegRunner {
    async fn run(&self, args: &[String]) -> Result<FfmpegOutput, ServiceError> {
        let output = Command::new(&self.binary)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;
        Ok(FfmpegOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code().unwrap_or(-1),
        })
    }
}

/// 测试用执行器——按构造时注入的闭包返回 fixture 输出。
///
/// 闭包接收完整 `args` 切片，可据 args 内容（如目标高度）返回不同 fixture。
/// 默认返回 [`FfmpegOutput::ok`]（成功空输出）。
pub struct FixtureFfmpegRunner {
    /// 注入的 fixture 闭包（args → 输出）。`Box<dyn Send + Sync>` 满足 trait 约束。
    fixture: FixtureFn,
    /// 最近一次调用的 args（测试断言用）。
    last_args: std::sync::Mutex<Option<Vec<String>>>,
}

/// fixture 闭包类型（args 切片 → 输出；`Send + Sync` 满足 trait 约束）。
type FixtureFn = Box<dyn Fn(&[String]) -> FfmpegOutput + Send + Sync>;

impl FixtureFfmpegRunner {
    /// 用固定 fixture 构造（任何调用都返回同一输出；默认 [`FfmpegOutput::ok`]）。
    pub fn new(output: FfmpegOutput) -> Self {
        Self {
            fixture: Box::new(move |_| output.clone()),
            last_args: std::sync::Mutex::new(None),
        }
    }

    /// 用闭包构造（按 args 动态返回 fixture）。
    pub fn with_fn<F>(fixture: F) -> Self
    where
        F: Fn(&[String]) -> FfmpegOutput + Send + Sync + 'static,
    {
        Self {
            fixture: Box::new(fixture),
            last_args: std::sync::Mutex::new(None),
        }
    }

    /// 取最近一次调用的 args（测试断言用）；None 表示尚未被调用。
    pub fn last_args(&self) -> Option<Vec<String>> {
        self.last_args.lock().expect("fixture lock").clone()
    }
}

#[async_trait]
impl FfmpegRunner for FixtureFfmpegRunner {
    async fn run(&self, args: &[String]) -> Result<FfmpegOutput, ServiceError> {
        *self.last_args.lock().expect("fixture lock") = Some(args.to_vec());
        Ok((self.fixture)(args))
    }
}

// ----------------------------------------------------------------------------
// TranscodeProfile → FFmpeg 参数映射
// ----------------------------------------------------------------------------

/// HLS 段时长（秒）。Apple 推荐 6s（2s GOP × 3）；与 `media_impl` 中注释一致。
pub const HLS_SEGMENT_SECS: u32 = 6;

/// 一个 HLS 转码变体（用于 ABR master playlist 编排）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsVariant {
    /// 档位（不含 Original；Original 不转码）。
    pub profile: TranscodeProfile,
    /// media playlist 文件名（如 `"720p.m3u8"`）。
    pub playlist_filename: String,
    /// 可选密钥文件路径（HLS-AES 加密；留接口点，当前编排不写入）。
    pub key_file: Option<PathBuf>,
}

impl HlsVariant {
    /// 构造一个无加密的变体。
    pub fn new(profile: TranscodeProfile, playlist_filename: impl Into<String>) -> Self {
        Self {
            profile,
            playlist_filename: playlist_filename.into(),
            key_file: None,
        }
    }

    /// 按 profile 默认命名变体（`"1080p.m3u8"` 等）。
    pub fn from_profile(profile: TranscodeProfile) -> Self {
        let name = match profile {
            TranscodeProfile::Hls1080p => "1080p.m3u8",
            TranscodeProfile::Hls720p => "720p.m3u8",
            TranscodeProfile::Hls480p => "480p.m3u8",
            TranscodeProfile::Original => "original.m3u8",
        };
        Self::new(profile, name)
    }
}

/// 构造 FFmpeg HLS 转码命令的参数向量（不含 `ffmpeg` 本身）。
///
/// 生成的命令形如（720p）：
/// ```text
/// -y -i <input>
/// -vf scale=-2:720
/// -c:v libx264 -preset veryfast -b:v 2800k -maxrate 2996k -bufsize 5600k
/// -c:a aac -b:a 128k
/// -hls_time 6 -hls_playlist_type vod -hls_segment_filename <dir>/720p_%05d.ts
/// -f hls <dir>/720p.m3u8
/// ```
///
/// `-vf scale=-2:<h>`：高度固定为 `h`，宽度按比例自动（`-2` 保证偶数，H.264 要求）。
/// 码率策略：`b:v` = 目标码率；`maxrate` = `b:v × 1.07`（HLS 推荐 buffer 一致性）；
/// `bufsize` = `b:v × 2`（VBV 缓冲）。
///
/// `Original` 档位不转码（`-c copy`），但仍封装为 HLS（流式复制到 ts + m3u8）。
pub fn build_hls_args(
    input: &Path,
    output_dir: &Path,
    variant: &HlsVariant,
    segment_secs: u32,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::with_capacity(24);

    // -y：覆盖输出（重新转码场景；vod 产物幂等）
    args.push("-y".into());
    args.push("-i".into());
    args.push(input.to_string_lossy().into_owned());

    let height = variant.profile.target_height();
    let bitrate_bps = variant.profile.target_bitrate_bps();

    match variant.profile {
        TranscodeProfile::Original => {
            // 流复制（不重编码）：视频/音频原样打包到 HLS。
            args.push("-c".into());
            args.push("copy".into());
        }
        _ => {
            // 视频滤镜：固定高度，宽度按比例（-2 保证偶数）。
            args.push("-vf".into());
            args.push(format!("scale=-2:{height}"));

            // 视频编码：H.264 + veryfast 预设（实时性 / 转码吞吐平衡）。
            args.push("-c:v".into());
            args.push("libx264".into());
            args.push("-preset".into());
            args.push("veryfast".into());

            // 码率控制（kbps）：目标 + maxrate（一致性）+ bufsize（VBV 缓冲）。
            let bps_k = bitrate_bps / 1_000;
            let maxrate_k = (bitrate_bps as f64 * 1.07 / 1_000.0).round() as u64;
            let bufsize_k = bitrate_bps * 2 / 1_000;
            args.push("-b:v".into());
            args.push(format!("{bps_k}k"));
            args.push("-maxrate".into());
            args.push(format!("{maxrate_k}k"));
            args.push("-bufsize".into());
            args.push(format!("{bufsize_k}k"));

            // 音频编码：AAC 128 kbps（HLS 推荐基线）。
            args.push("-c:a".into());
            args.push("aac".into());
            args.push("-b:a".into());
            args.push("128k".into());
        }
    }

    // HLS muxer 选项：段时长、playlist 类型（vod = 点播，事件不可变）。
    args.push("-hls_time".into());
    args.push(segment_secs.to_string());
    args.push("-hls_playlist_type".into());
    args.push("vod".into());

    // 段文件命名：<playlist_basename>_%05d.ts
    let stem = playlist_stem(&variant.playlist_filename);
    let seg_pattern = output_dir
        .join(format!("{stem}_%05d.ts"))
        .to_string_lossy()
        .into_owned();
    args.push("-hls_segment_filename".into());
    args.push(seg_pattern);

    args.push("-f".into());
    args.push("hls".into());

    // 输出：media playlist 文件（位于 output_dir 下）。
    let out = output_dir
        .join(&variant.playlist_filename)
        .to_string_lossy()
        .into_owned();
    args.push(out);

    args
}

/// 从 playlist 文件名取 stem（去扩展名），用作段文件前缀。
fn playlist_stem(playlist_filename: &str) -> &str {
    Path::new(playlist_filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(playlist_filename)
}

// ----------------------------------------------------------------------------
// HLS playlist 生成（纯函数；用于不真跑 FFmpeg 时构造产物）
// ----------------------------------------------------------------------------

/// 生成 master playlist（ABR 多档位），文本为标准 m3u8。
///
/// 包含所有变体的 `#EXT-X-STREAM-INF`（带 BANDWIDTH + RESOLUTION），
/// 指向各档位的 media playlist。原始档位用 `#EXT-X-STREAM-INF:BANDWIDTH=...`
/// 标注（无分辨率缩放，但保留带宽估算）。
pub fn build_master_playlist(variants: &[HlsVariant]) -> String {
    let mut s = String::with_capacity(256);
    s.push_str("#EXTM3U\n");
    s.push_str("#EXT-X-VERSION:3\n");

    for v in variants {
        let bw = v.profile.target_bitrate_bps();
        let h = v.profile.target_height();
        // 分辨率宽估算：16:9 长宽比，宽度 = h × 16 / 9（保证整数）。
        let w = if h == 0 {
            0
        } else {
            (h as f64 * 16.0 / 9.0).round() as u32
        };
        match v.profile {
            TranscodeProfile::Original => {
                // 无分辨率约束；只标 BANDWIDTH（用估算码率，Original 实际 0 → 用 0）。
                s.push_str(&format!(
                    "#EXT-X-STREAM-INF:BANDWIDTH={bw}\n{}\n",
                    v.playlist_filename
                ));
            }
            _ => {
                let name = &v.playlist_filename;
                s.push_str(&format!(
                    "#EXT-X-STREAM-INF:BANDWIDTH={bw},RESOLUTION={w}x{h}\n{name}\n"
                ));
            }
        }
    }
    s
}

/// 生成 media playlist（单档位的 vod 点播清单）。
///
/// `segment_count` 个段，每段 `segment_secs` 秒，命名 `<stem>_%05d.ts`。
/// 最后追加 `#EXT-X-ENDLIST`（vod 标志）。
pub fn build_media_playlist(variant: &HlsVariant, segment_count: u32, segment_secs: f64) -> String {
    let stem = playlist_stem(&variant.playlist_filename);
    let mut s = String::with_capacity(128 + 32 * segment_count as usize);
    s.push_str("#EXTM3U\n");
    s.push_str("#EXT-X-VERSION:3\n");
    s.push_str(&format!(
        "#EXT-X-TARGETDURATION:{}\n",
        segment_secs.ceil() as u32
    ));
    s.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");

    for i in 0..segment_count {
        s.push_str(&format!("#EXTINF:{:.3},\n", segment_secs));
        s.push_str(&format!("{stem}_{i:05}.ts\n"));
    }
    s.push_str("#EXT-X-ENDLIST\n");
    s
}

// ----------------------------------------------------------------------------
// 转码执行编排
// ----------------------------------------------------------------------------

/// 执行单档位 HLS 转码：构造命令 → 调 runner → 失败映射错误。
///
/// 成功后产物（m3u8 + ts 段）已写入 `output_dir`。返回 fixture/真实 ffmpeg 的输出。
/// 非零退出 → `ServiceError::Internal`（保留 stderr 诊断）。
///
/// **不**在此处创建目录——由调用方（[`transcode_abr`]）负责 `mkdir -p output_dir`，
/// 避免重复 IO 与权限竞争。
pub async fn transcode_variant(
    runner: &dyn FfmpegRunner,
    input: &Path,
    output_dir: &Path,
    variant: &HlsVariant,
    segment_secs: u32,
) -> Result<FfmpegOutput, ServiceError> {
    let args = build_hls_args(input, output_dir, variant, segment_secs);
    let out = runner.run(&args).await?;
    if out.exit_code != 0 {
        return Err(ServiceError::Internal(format!(
            "ffmpeg 退出码 {}：{}",
            out.exit_code,
            out.stderr.trim()
        )));
    }
    Ok(out)
}

/// 执行 ABR 多档位 HLS 转码 + 生成 master playlist。
///
/// - 为每个 variant 构造命令并 spawn（**顺序执行**——FFmpeg 单进程已吃满 CPU；
///   并行转码会过载；并行优化留 TODO \[RUNTIME\]——需真实 ffmpeg 子进程 + 资源画像调优）。
/// - 全部成功后写 `master.m3u8`（由 [`build_master_playlist`]）到 `output_dir`。
/// - 任一档位失败立即返回错误（已转档位产物保留，由调用方清理策略决定）。
///
/// 返回 master playlist 文本（调用方可直接持久化或反代）。
pub async fn transcode_abr(
    runner: &dyn FfmpegRunner,
    input: &Path,
    output_dir: &Path,
    variants: &[HlsVariant],
    segment_secs: u32,
) -> Result<String, ServiceError> {
    if variants.is_empty() {
        return Err(ServiceError::Internal(
            "ABR 转码至少需要一个 variant".into(),
        ));
    }

    // 确保输出目录存在（生产应在此；测试可预先建）。
    tokio::fs::create_dir_all(output_dir).await?;

    for v in variants {
        transcode_variant(runner, input, output_dir, v, segment_secs).await?;
    }

    let master = build_master_playlist(variants);
    // 写 master.m3u8（与变体 media playlist 同目录）。
    let master_path = output_dir.join("master.m3u8");
    tokio::fs::write(&master_path, &master).await?;
    Ok(master)
}

// ============================================================================
// 单元测试
// ============================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::TranscodeProfile::*;

    fn variant(p: TranscodeProfile) -> HlsVariant {
        HlsVariant::from_profile(p)
    }

    // —— 命令构造 ——

    #[test]
    fn build_hls_args_720p_includes_scale_and_bitrate() {
        let v = variant(Hls720p);
        let args = build_hls_args(
            Path::new("/in/a.mp4"),
            Path::new("/out"),
            &v,
            HLS_SEGMENT_SECS,
        );
        // 必须包含的关键参数
        assert!(args.contains(&"-y".to_string()), "覆盖输出 -y");
        assert!(args.contains(&"-i".to_string()), "input 旗标");
        assert!(args.contains(&"/in/a.mp4".to_string()), "input 路径");
        assert!(args.contains(&"-vf".to_string()), "video filter");
        assert!(args.contains(&"scale=-2:720".to_string()), "720p 缩放");
        assert!(args.contains(&"libx264".to_string()), "H.264 编码");
        assert!(args.contains(&"aac".to_string()), "AAC 音频");
        // 720p 目标码率 2_800_000 bps → 2800k
        assert!(args.contains(&"2800k".to_string()), "目标码率 2800k");
        // HLS muxer
        assert!(args.contains(&"-hls_time".to_string()));
        assert!(args.contains(&"6".to_string()), "6 秒段");
        assert!(args.contains(&"-hls_playlist_type".to_string()));
        assert!(args.contains(&"vod".to_string()));
        assert!(args.contains(&"-f".to_string()));
        assert!(args.contains(&"hls".to_string()));
        // 输出 playlist + 段命名
        assert!(
            args.iter().any(|a| a.ends_with("720p.m3u8")),
            "输出 playlist"
        );
        assert!(
            args.iter().any(|a| a.contains("720p_%05d.ts")),
            "段命名模式"
        );
    }

    #[test]
    fn build_hls_args_1080p_bitrate_correct() {
        let v = variant(Hls1080p);
        let args = build_hls_args(Path::new("/in.mp4"), Path::new("/o"), &v, HLS_SEGMENT_SECS);
        assert!(args.contains(&"scale=-2:1080".to_string()));
        // 5_000_000 bps → 5000k
        assert!(args.contains(&"5000k".to_string()));
        // maxrate = 5000k × 1.07 ≈ 5350k
        assert!(args.contains(&"5350k".to_string()), "maxrate = b:v × 1.07");
        // bufsize = b:v × 2 = 10000k
        assert!(args.contains(&"10000k".to_string()), "bufsize = b:v × 2");
    }

    #[test]
    fn build_hls_args_480p_bitrate_correct() {
        let v = variant(Hls480p);
        let args = build_hls_args(Path::new("/in.mp4"), Path::new("/o"), &v, HLS_SEGMENT_SECS);
        assert!(args.contains(&"scale=-2:480".to_string()));
        // 1_400_000 bps → 1400k
        assert!(args.contains(&"1400k".to_string()));
    }

    #[test]
    fn build_hls_args_original_uses_copy() {
        let v = variant(Original);
        let args = build_hls_args(Path::new("/in.mp4"), Path::new("/o"), &v, HLS_SEGMENT_SECS);
        // Original 不重编码：-c copy
        assert!(args.contains(&"-c".to_string()));
        assert!(args.contains(&"copy".to_string()));
        // 不应有 libx264
        assert!(!args.contains(&"libx264".to_string()));
        // 不应有 scale 滤镜
        assert!(
            !args.iter().any(|a| a.starts_with("scale=")),
            "Original 不下采样"
        );
        // 但仍要 HLS muxer + 段命名
        assert!(args.contains(&"-hls_time".to_string()));
        assert!(
            args.iter().any(|a| a.ends_with("original.m3u8")),
            "输出 playlist"
        );
    }

    #[test]
    fn build_hls_args_custom_segment_secs() {
        let v = variant(Hls720p);
        let args = build_hls_args(Path::new("/i"), Path::new("/o"), &v, 4);
        assert!(args.contains(&"-hls_time".to_string()));
        assert!(args.contains(&"4".to_string()), "自定义 4 秒段");
    }

    #[test]
    fn playlist_stem_strips_extension() {
        assert_eq!(playlist_stem("720p.m3u8"), "720p");
        assert_eq!(playlist_stem("a/b/1080p.m3u8"), "1080p");
        assert_eq!(playlist_stem("noext"), "noext");
    }

    // —— HLS playlist 生成 ——

    #[test]
    fn master_playlist_lists_all_variants() {
        let vs = vec![variant(Hls1080p), variant(Hls720p), variant(Hls480p)];
        let m = build_master_playlist(&vs);
        assert!(m.starts_with("#EXTM3U\n"));
        assert!(m.contains("#EXT-X-VERSION:3\n"));
        // 每个变体各一行 playlist 引用 + 一行 STREAM-INF
        assert!(m.contains("1080p.m3u8"));
        assert!(m.contains("720p.m3u8"));
        assert!(m.contains("480p.m3u8"));
        // 分辨率标注（720p → 1280x720，16:9）
        assert!(m.contains("RESOLUTION=1280x720"));
        assert!(m.contains("RESOLUTION=1920x1080"));
        assert!(m.contains("RESOLUTION=853x480"));
        // 带宽标注
        assert!(m.contains("BANDWIDTH=2800000"));
    }

    #[test]
    fn master_playlist_resolution_is_16_to_9() {
        // 720p: 720 × 16/9 = 1280
        let v = variant(Hls720p);
        let m = build_master_playlist(&[v]);
        assert!(m.contains("RESOLUTION=1280x720"));
    }

    #[test]
    fn master_playlist_empty_variants() {
        let m = build_master_playlist(&[]);
        assert!(m.starts_with("#EXTM3U\n"));
        // 仅头，无 STREAM-INF
        assert!(!m.contains("EXT-X-STREAM-INF"));
    }

    #[test]
    fn media_playlist_segments_and_endlist() {
        let v = variant(Hls720p);
        let m = build_media_playlist(&v, 3, 6.0);
        assert!(m.starts_with("#EXTM3U\n"));
        assert!(m.contains("#EXT-X-VERSION:3\n"));
        assert!(m.contains("#EXT-X-TARGETDURATION:6\n"));
        assert!(m.contains("#EXT-X-PLAYLIST-TYPE:VOD\n"));
        // 3 个段：720p_00000.ts / 00001 / 00002
        assert!(m.contains("720p_00000.ts"));
        assert!(m.contains("720p_00001.ts"));
        assert!(m.contains("720p_00002.ts"));
        // EXTINF 时长（浮点）
        assert!(m.contains("#EXTINF:6.000,\n"));
        // 结束标志
        assert!(m.contains("#EXT-X-ENDLIST\n"));
    }

    #[test]
    fn media_playlist_zero_segments() {
        let v = variant(Hls720p);
        let m = build_media_playlist(&v, 0, 6.0);
        // 0 段但仍合法 vod 清单（仅头 + ENDLIST）
        assert!(m.contains("#EXT-X-ENDLIST\n"));
        assert!(!m.contains("720p_00000.ts"));
    }

    #[test]
    fn media_playlist_fractional_segment_duration() {
        let v = variant(Hls720p);
        let m = build_media_playlist(&v, 1, 6.5);
        // 6.5 秒段 → targetduration 取 ceil = 7
        assert!(m.contains("#EXT-X-TARGETDURATION:7\n"));
        assert!(m.contains("#EXTINF:6.500,\n"));
    }

    // —— FfmpegRunner fixture ——

    #[tokio::test]
    async fn fixture_runner_returns_injected_output() {
        let runner = FixtureFfmpegRunner::new(FfmpegOutput {
            exit_code: 0,
            stdout: "ok".into(),
            stderr: String::new(),
        });
        let out = runner.run(&["-version".into()]).await.unwrap();
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout, "ok");
        // last_args 记录
        assert_eq!(runner.last_args(), Some(vec!["-version".to_string()]));
    }

    #[tokio::test]
    async fn fixture_runner_with_fn_dynamic() {
        // 按 args 内容动态返回不同 fixture（模拟成功 + 失败）
        let runner = FixtureFfmpegRunner::with_fn(|args| {
            if args.iter().any(|a| a == "scale=-2:720") {
                FfmpegOutput::ok()
            } else {
                FfmpegOutput {
                    exit_code: 1,
                    stdout: String::new(),
                    stderr: "Unknown profile".into(),
                }
            }
        });
        let ok_args = build_hls_args(
            Path::new("/i"),
            Path::new("/o"),
            &variant(Hls720p),
            HLS_SEGMENT_SECS,
        );
        let out = runner.run(&ok_args).await.unwrap();
        assert_eq!(out.exit_code, 0);

        let bad_args = build_hls_args(
            Path::new("/i"),
            Path::new("/o"),
            &variant(Original),
            HLS_SEGMENT_SECS,
        );
        let out = runner.run(&bad_args).await.unwrap();
        assert_eq!(out.exit_code, 1);
    }

    #[tokio::test]
    async fn transcode_variant_success() {
        let runner = FixtureFfmpegRunner::new(FfmpegOutput::ok());
        let dir = std::env::temp_dir();
        let out = transcode_variant(
            &runner,
            Path::new("/in/a.mp4"),
            &dir,
            &variant(Hls720p),
            HLS_SEGMENT_SECS,
        )
        .await
        .unwrap();
        assert_eq!(out.exit_code, 0);
        // fixture 应记录了构造出的 args（含 scale=-2:720）
        let recorded = runner.last_args().unwrap();
        assert!(recorded.contains(&"scale=-2:720".to_string()));
    }

    #[tokio::test]
    async fn transcode_variant_failure_maps_to_internal() {
        let runner = FixtureFfmpegRunner::new(FfmpegOutput {
            exit_code: 1,
            stdout: String::new(),
            stderr: "Invalid data found".into(),
        });
        let err = transcode_variant(
            &runner,
            Path::new("/in/a.mp4"),
            std::env::temp_dir().as_path(),
            &variant(Hls720p),
            HLS_SEGMENT_SECS,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ServiceError::Internal(_)));
        let msg = format!("{err}");
        assert!(msg.contains("Invalid data found"), "保留 stderr 诊断");
    }

    #[tokio::test]
    async fn transcode_abr_writes_master_playlist() {
        let runner = FixtureFfmpegRunner::new(FfmpegOutput::ok());
        let dir = std::env::temp_dir().join(format!(
            "ffmpeg-abr-test-{}-{}",
            std::process::id(),
            uuid_counter()
        ));
        let variants = vec![variant(Hls720p), variant(Hls480p)];
        let master = transcode_abr(
            &runner,
            Path::new("/in/a.mp4"),
            &dir,
            &variants,
            HLS_SEGMENT_SECS,
        )
        .await
        .unwrap();
        // master 内容正确
        assert!(master.contains("720p.m3u8"));
        assert!(master.contains("480p.m3u8"));
        // master.m3u8 文件已落盘
        let written = std::fs::read_to_string(dir.join("master.m3u8")).unwrap();
        assert_eq!(written, master);
        // 清理
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn transcode_abr_empty_variants_errors() {
        let runner = FixtureFfmpegRunner::new(FfmpegOutput::ok());
        let err = transcode_abr(
            &runner,
            Path::new("/in/a.mp4"),
            std::env::temp_dir().as_path(),
            &[],
            HLS_SEGMENT_SECS,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ServiceError::Internal(_)));
    }

    #[test]
    fn tokio_runner_default_binary_is_ffmpeg() {
        let r = TokioFfmpegRunner::new();
        assert_eq!(r.binary(), "ffmpeg");
        let r2 = TokioFfmpegRunner::with_binary("/usr/local/bin/ffmpeg");
        assert_eq!(r2.binary(), "/usr/local/bin/ffmpeg");
    }

    fn uuid_counter() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static C: AtomicU64 = AtomicU64::new(0);
        C.fetch_add(1, Ordering::SeqCst)
    }
}
