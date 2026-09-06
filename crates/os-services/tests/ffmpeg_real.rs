//! media_ffmpeg 真实转码测——本机真实 FFmpeg 8.0.1 子进程跑通。
//!
//! **定位**：补充 `src/media_ffmpeg.rs` 的单元测——单元测用 `FixtureFfmpegRunner`
//! 验证命令构造逻辑，但从未真正 spawn ffmpeg。本文件用 `TokioFfmpegRunner` 跑真实
//! 子进程，验证：
//! - ffmpeg 二进制可达（`-version`）；
//! - `build_hls_args` 构造的命令被真实 ffmpeg 接受并产出合法 HLS（m3u8 + ts）；
//! - 非法输入时 ffmpeg 非零退出码被 `TokioFfmpegRunner` 正确传播 + stderr 保留；
//! - ABR 多档位转码产出 master + 多个 media playlist。
//!
//! **环境要求**：`ffmpeg`（含 libx264 编码器）+ `ffprobe`（可选，用于产物校验）
//! 在 `$PATH`。本机已装 ffmpeg 8.0.1（`--enable-libx264`）。
//!
//! **运行**（默认套件不跑，全部 `#[ignore]`）：
//! ```bash
//! cargo test -p os-services --features mock --test ffmpeg_real -- --ignored --nocapture
//! ```
//! 无 ffmpeg 环境优雅 SKIP（见 [`ffmpeg_reachable`]）。
//!
//! **产物清理**：所有 `/tmp/osprobe_ffmpeg_<pid>/` 由 [`TmpProbeDir`] RAII guard
//! 在 drop 时清理（即便断言失败也保证清理）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use os_services::media::TranscodeProfile;
use os_services::{
    build_hls_args, build_master_playlist, transcode_abr, FfmpegRunner, HlsVariant,
    TokioFfmpegRunner, HLS_SEGMENT_SECS,
};

// ----------------------------------------------------------------------------
// 通用辅助：跳过 / 临时目录 / 探测
// ----------------------------------------------------------------------------

/// 临时工作目录 RAII guard：drop 时递归删除 `/tmp/osprobe_ffmpeg_<pid>_<n>/`。
///
/// 即使测试断言 panic，guard 的 drop 仍会运行（panic unwind），保证产物清理。
struct TmpProbeDir {
    path: PathBuf,
}

impl TmpProbeDir {
    /// 创建 `/tmp/osprobe_ffmpeg_<pid>_<counter>/`，返回 guard。
    fn new() -> Self {
        let n = DIR_COUNTER.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("osprobe_ffmpeg_{}_{}", std::process::id(), n));
        // 幂等创建；真实转码前必须存在（transcode_abr 也会建，但单独测建一次更稳）。
        std::fs::create_dir_all(&path).expect("创建临时目录失败");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TmpProbeDir {
    fn drop(&mut self) {
        // 清理失败不 panic（测试结果优先），仅 stderr 提示。
        if let Err(e) = std::fs::remove_dir_all(&self.path) {
            eprintln!("[ffmpeg_real] 清理 {} 失败: {}", self.path.display(), e);
        }
    }
}

/// 目录计数器（同进程多个 TmpProbeDir 互不冲突）。
static DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 探测本机 ffmpeg 是否可达（`-version` 退出码 0）。
///
/// 用 `TokioFfmpegRunner::new()` 跑 `ffmpeg -version`；非零退出或 spawn 失败
/// （二进制不在 `$PATH`）均视为不可达。所有真实测开头 `await` 此函数，不可达时
/// `eprintln` + 提前返回（不 panic），由 `--ignored` 调用方看到 SKIP。
///
/// **异步**：由 `#[tokio::test]` 提供的 runtime 驱动（避免在 runtime 内嵌套 block_on）。
async fn ffmpeg_reachable() -> bool {
    let runner = TokioFfmpegRunner::new();
    let args: Vec<String> = vec!["-version".into()];
    match runner.run(&args).await {
        Ok(out) => out.exit_code == 0,
        Err(_) => false,
    }
}

/// SKIP 宏风格：不可达时打印 + 返回（不再往下断言）。
/// 必须在 async 函数体内使用（展开含 `.await`）。
macro_rules! require_ffmpeg {
    () => {
        if !ffmpeg_reachable().await {
            eprintln!("[ffmpeg_real] SKIP: ffmpeg 不可达（未装 / 不在 $PATH）");
            return;
        }
    };
}

/// 探测 ffprobe 是否可达（可选，用于产物校验）。
async fn ffprobe_reachable() -> bool {
    let runner = TokioFfmpegRunner::with_binary("ffprobe");
    let args: Vec<String> = vec!["-version".into()];
    match runner.run(&args).await {
        Ok(out) => out.exit_code == 0,
        Err(_) => false,
    }
}

/// 用 `ffmpeg lavfi testsrc` 生成一个 2 秒 320x240 测试视频到 `dst`。
///
/// 命令：`ffmpeg -y -f lavfi -i testsrc=duration=2:size=320x240:rate=15 -c:v libx264 <dst>`。
/// 返回 ffmpeg 输出（exit_code == 0 表示成功）。
async fn gen_test_video(runner: &dyn FfmpegRunner, dst: &Path) -> os_core::CommandOutput {
    let dst_str = dst.to_string_lossy().into_owned();
    let args: Vec<String> = vec![
        "-y".into(),
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        "testsrc=duration=2:size=320x240:rate=15".into(),
        // 加默认 avalsinsrc 音频以覆盖 -c:a 路径（避免 ffmpeg 抱怨无音频流）。
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        "sine=frequency=440:duration=2".into(),
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "veryfast".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-shortest".into(),
        dst_str,
    ];
    runner.run(&args).await.expect("gen_test_video spawn 失败")
}

// ----------------------------------------------------------------------------
// 真实测 a：ffmpeg 可达性（-version）
// ----------------------------------------------------------------------------

/// 跑 `TokioFfmpegRunner::new()` + `ffmpeg -version`，断言退出码 0 + stdout 含 "ffmpeg version"。
#[tokio::test]
#[ignore = "真实测：需本机 ffmpeg（cargo test -- --ignored --nocapture）"]
async fn real_ffmpeg_version_reachable() {
    require_ffmpeg!();

    let runner = TokioFfmpegRunner::new();
    let args: Vec<String> = vec!["-version".into()];
    let out = runner
        .run(&args)
        .await
        .expect("run ffmpeg -version 失败（spawn）");

    assert_eq!(out.exit_code, 0, "ffmpeg -version 应退出码 0");
    assert!(
        out.stdout.contains("ffmpeg version"),
        "stdout 应含 'ffmpeg version'；实际 stdout 首 200 字：\n{}",
        out.stdout.chars().take(200).collect::<String>()
    );
    // ffmpeg -version 输出到 stdout（8.0.1 验证）；stderr 可能为空或含配置。
    println!(
        "[ffmpeg_real] ffmpeg 可达，版本头：{}",
        out.stdout.lines().next().unwrap_or("")
    );
}

// ----------------------------------------------------------------------------
// 真实测 b：真实转码 HLS（testsrc → m3u8 + ts）
// ----------------------------------------------------------------------------

/// 用 lavfi testsrc 生成 2 秒测试视频 → 用 `build_hls_args` 构造命令 → 真实转码
/// 为 HLS（720p）→ 断言 `.m3u8` + `.ts` 文件真实存在 + 非空 + `#EXTM3U` 头。
///
/// 验证的关键路径：`build_hls_args` 构造的 argv 被 **真实 ffmpeg** 接受并产出合法 HLS。
/// 这是 `media_ffmpeg.rs` 中从未被本机验证过的核心逻辑。
#[tokio::test]
#[ignore = "真实测：需本机 ffmpeg + libx264（cargo test -- --ignored --nocapture）"]
async fn real_transcode_hls_single_variant() {
    require_ffmpeg!();

    let _guard = TmpProbeDir::new();
    let dir = _guard.path().to_path_buf();
    let src = dir.join("src.mp4");

    // 1. 生成 2 秒测试视频。
    let runner = TokioFfmpegRunner::new();
    let gen = gen_test_video(&runner, &src).await;
    assert_eq!(
        gen.exit_code, 0,
        "生成测试视频失败，stderr:\n{}",
        gen.stderr
    );
    assert!(src.exists(), "测试视频应已生成");
    assert!(src.metadata().unwrap().len() > 0, "测试视频应非空");

    // 2. 用 build_hls_args 构造转码命令（720p），跑真实 ffmpeg。
    let variant = HlsVariant::from_profile(TranscodeProfile::Hls720p);
    let args = build_hls_args(&src, &dir, &variant, HLS_SEGMENT_SECS);
    let out = runner.run(&args).await.expect("run transcode 失败");
    assert_eq!(out.exit_code, 0, "HLS 转码应成功，stderr:\n{}", out.stderr);

    // 3. 断言产物：m3u8 + ts 真实存在 + 非空。
    let m3u8 = dir.join("720p.m3u8");
    assert!(m3u8.exists(), "720p.m3u8 应存在");
    let m3u8_text = std::fs::read_to_string(&m3u8).unwrap();
    assert!(!m3u8_text.is_empty(), "m3u8 应非空");
    assert!(
        m3u8_text.starts_with("#EXTM3U"),
        "m3u8 应以 #EXTM3U 开头；实际首行：{:?}",
        m3u8_text.lines().next()
    );
    // vod playlist 应含 ENDLIST。
    assert!(
        m3u8_text.contains("#EXT-X-ENDLIST"),
        "vod playlist 应含 #EXT-X-ENDLIST"
    );

    // 至少一个 ts 段（testsrc 2 秒 + hls_time 6 → 单段）。
    let ts_files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("ts"))
        .collect();
    assert!(
        !ts_files.is_empty(),
        "应至少有一个 .ts 段；目录内容：{:?}",
        std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
    );
    for ts in &ts_files {
        let len = ts.metadata().unwrap().len();
        assert!(len > 0, "ts 段 {} 应非空", ts.path().display());
    }
    println!(
        "[ffmpeg_real] 单档位 HLS 转码成功：m3u8={} 字节，{} 个 ts 段",
        m3u8_text.len(),
        ts_files.len()
    );

    // 4. （可选）ffprobe 校验 m3u8 是合法 HLS。
    if ffprobe_reachable().await {
        let probe = TokioFfmpegRunner::with_binary("ffprobe");
        let probe_args: Vec<String> = vec![
            "-v".into(),
            "error".into(),
            "-show_entries".into(),
            "format=format_name".into(),
            "-of".into(),
            "default=noprint_wrappers=1".into(),
            m3u8.to_string_lossy().into_owned(),
        ];
        let po = probe.run(&probe_args).await.unwrap();
        assert_eq!(po.exit_code, 0, "ffprobe 应成功（m3u8 合法）");
        assert!(
            po.stdout.contains("hls") || po.stdout.contains("mov,mp4"),
            "ffprobe 应识别为 hls；实际 stdout: {}",
            po.stdout
        );
        println!("[ffmpeg_real] ffprobe 校验 m3u8 合法：{}", po.stdout.trim());
    } else {
        eprintln!("[ffmpeg_real] ffprobe 不可达，跳过 m3u8 格式校验");
    }
}

// ----------------------------------------------------------------------------
// 真实测 c：转码命令构造验证（FixtureRunner 捕获 argv）
// ----------------------------------------------------------------------------

/// 用 `FixtureFfmpegRunner` 注入闭包，捕获 `build_hls_args` 构造的完整 argv，
/// 断言含 `-vf scale`/`-c:v libx264`/HLS 关键参数。
///
/// 这是纯逻辑测（不 spawn），但用真实 build_hls_args + 真实 transcode_variant 路径，
/// 验证「构造 → 调 runner → 接收 argv」链路完整。**不**带 `#[ignore]`（无外部依赖）。
#[tokio::test]
async fn fixture_runner_captures_correct_hls_args() {
    use os_services::FixtureFfmpegRunner;

    let captured: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let captured_clone = captured.clone();
    let runner = FixtureFfmpegRunner::with_fn(move |args| {
        *captured_clone.lock().unwrap() = args.to_vec();
        os_core::CommandOutput::ok()
    });

    let variant = HlsVariant::from_profile(TranscodeProfile::Hls720p);
    let dir = std::env::temp_dir().join("osprobe_args_fixture");
    let _g = scope_guard(&dir);
    let input = Path::new("/tmp/test_input.mp4");

    // transcode_variant 内部调 build_hls_args → runner.run。
    let out = os_services::transcode_variant(&runner, input, &dir, &variant, HLS_SEGMENT_SECS)
        .await
        .expect("transcode_variant 应成功（fixture）");
    assert_eq!(out.exit_code, 0);

    let args = captured.lock().unwrap().clone();
    // 关键参数断言。
    assert!(args.contains(&"-y".to_string()), "应有 -y");
    assert!(args.contains(&"-i".to_string()), "应有 -i");
    assert!(
        args.iter().any(|a| a.starts_with("scale=-2:720")),
        "应有 scale=-2:720，实际 args: {:?}",
        args
    );
    assert!(args.contains(&"-vf".to_string()));
    assert!(args.contains(&"-c:v".to_string()));
    assert!(args.contains(&"libx264".to_string()), "应有 libx264");
    assert!(args.contains(&"-c:a".to_string()));
    assert!(args.contains(&"aac".to_string()), "音频应为 aac");
    assert!(args.contains(&"-hls_time".to_string()));
    assert!(args.contains(&"-hls_playlist_type".to_string()));
    assert!(args.contains(&"vod".to_string()));
    assert!(args.contains(&"-f".to_string()));
    assert!(args.contains(&"hls".to_string()));
    // 段命名模式 + 输出 m3u8。
    assert!(
        args.iter().any(|a| a.contains("720p_%05d.ts")),
        "应有段命名模式 720p_%05d.ts"
    );
    assert!(
        args.iter().any(|a| a.ends_with("720p.m3u8")),
        "应输出 720p.m3u8"
    );
    // last_args 也应记录（FixtureFfmpegRunner 内置）。
    assert_eq!(runner.last_args(), Some(args.clone()));

    println!("[ffmpeg_real] 构造的 argv（{} 个参数）：", args.len());
    for (i, a) in args.iter().enumerate() {
        println!("  [{i:>2}] {a}");
    }
}

/// 极简 scope guard（避免引入 tempfile 到 dev-deps；仅创建 + drop 删除目录）。
fn scope_guard(dir: &Path) -> impl Drop {
    let _ = std::fs::create_dir_all(dir);
    let dir = dir.to_path_buf();
    struct G(PathBuf);
    impl Drop for G {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    G(dir)
}

// ----------------------------------------------------------------------------
// 真实测 d：错误处理（非法输入 → 非零退出码 + stderr 传播）
// ----------------------------------------------------------------------------

/// 故意传一个不存在的输入文件，断言 `TokioFfmpegRunner` 正确传播 ffmpeg 非零退出码，
/// 且 stderr 含 ffmpeg 的错误诊断（如 "No such file"）。
#[tokio::test]
#[ignore = "真实测：需本机 ffmpeg（cargo test -- --ignored --nocapture）"]
async fn real_ffmpeg_error_propagation_nonexistent_input() {
    require_ffmpeg!();

    let _guard = TmpProbeDir::new();
    let dir = _guard.path().to_path_buf();
    let bogus_input = dir.join("definitely_does_not_exist.mp4");
    // 不创建该文件——故意让 ffmpeg 找不到。

    let variant = HlsVariant::from_profile(TranscodeProfile::Hls720p);
    let args = build_hls_args(&bogus_input, &dir, &variant, HLS_SEGMENT_SECS);
    let runner = TokioFfmpegRunner::new();
    let out = runner.run(&args).await.expect("run 应成功 spawn");

    // ffmpeg 应非零退出（输入不存在）。
    assert_ne!(
        out.exit_code, 0,
        "ffmpeg 处理不存在的输入应非零退出；实际 exit_code={}，stderr:\n{}",
        out.exit_code, out.stderr
    );
    // stderr 应含诊断信息（ffmpeg 8.0 报 "No such file or directory" / "Could not open input"）。
    let diag = out.stderr.to_lowercase();
    assert!(
        diag.contains("no such file")
            || diag.contains("could not open input")
            || diag.contains("error"),
        "stderr 应含错误诊断；实际 stderr 末 500 字：\n{}",
        out.stderr
            .chars()
            .rev()
            .take(500)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>()
    );

    // 验证 transcode_variant 错误映射路径（ServiceError::Internal 含 stderr）。
    use os_services::ServiceError;
    let err =
        os_services::transcode_variant(&runner, &bogus_input, &dir, &variant, HLS_SEGMENT_SECS)
            .await
            .unwrap_err();
    assert!(matches!(err, ServiceError::Internal(_)));
    let msg = format!("{err}");
    // 诊断信息应被保留在错误消息里。
    assert!(
        msg.contains("退出码")
            || msg.to_lowercase().contains("no such file")
            || msg.to_lowercase().contains("could not open"),
        "错误消息应含诊断；实际: {msg}"
    );
    println!(
        "[ffmpeg_real] 非法输入错误传播成功：exit_code={}",
        out.exit_code
    );
}

// ----------------------------------------------------------------------------
// 真实测 e：ABR 多档位转码（master + 多 media playlist）
// ----------------------------------------------------------------------------

/// ABR 多档位 HLS 转码：720p + 480p → `transcode_abr` 产出 master.m3u8 + 两个 media playlist。
///
/// 验证：
/// - `transcode_abr` 用真实 ffmpeg 顺序转两档，全部成功；
/// - master.m3u8 落盘且含两个 STREAM-INF；
/// - 两个 media playlist（720p.m3u8 / 480p.m3u8）各自合法（#EXTM3U 头 + ENDLIST）。
#[tokio::test]
#[ignore = "真实测：需本机 ffmpeg + libx264（cargo test -- --ignored --nocapture）"]
async fn real_transcode_abr_multi_variant() {
    require_ffmpeg!();

    let _guard = TmpProbeDir::new();
    let dir = _guard.path().to_path_buf();
    let src = dir.join("src.mp4");

    // 1. 生成测试视频。
    let runner = TokioFfmpegRunner::new();
    let gen = gen_test_video(&runner, &src).await;
    assert_eq!(gen.exit_code, 0, "生成测试视频失败：{}", gen.stderr);

    // 2. ABR 多档位转码（720p + 480p）。
    let variants = vec![
        HlsVariant::from_profile(TranscodeProfile::Hls720p),
        HlsVariant::from_profile(TranscodeProfile::Hls480p),
    ];
    let master = transcode_abr(&runner, &src, &dir, &variants, HLS_SEGMENT_SECS)
        .await
        .expect("transcode_abr 应成功");
    // transcode_abr 内部已建目录 + 写 master.m3u8。

    // 3. master playlist 校验。
    assert!(master.starts_with("#EXTM3U"));
    assert!(master.contains("#EXT-X-VERSION:3"));
    // 两个 STREAM-INF 行（720p + 480p）。
    let stream_inf_count = master.matches("#EXT-X-STREAM-INF").count();
    assert_eq!(
        stream_inf_count, 2,
        "应有 2 个 STREAM-INF；实际 master:\n{master}"
    );
    assert!(master.contains("720p.m3u8"));
    assert!(master.contains("480p.m3u8"));
    // 分辨率标注。
    assert!(master.contains("RESOLUTION="));
    // master.m3u8 落盘。
    let master_file = dir.join("master.m3u8");
    assert!(master_file.exists(), "master.m3u8 应落盘");
    assert_eq!(std::fs::read_to_string(&master_file).unwrap(), master);

    // 4. 两个 media playlist 各自合法。
    for name in &["720p.m3u8", "480p.m3u8"] {
        let m = dir.join(name);
        assert!(m.exists(), "{name} 应存在");
        let text = std::fs::read_to_string(&m).unwrap();
        assert!(text.starts_with("#EXTM3U"), "{name} 应以 #EXTM3U 开头");
        assert!(text.contains("#EXT-X-ENDLIST"), "{name} 应含 ENDLIST");
        assert!(!text.is_empty(), "{name} 应非空");
        // 对应 ts 段应存在。
        let stem = name.strip_suffix(".m3u8").unwrap();
        let has_ts = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{stem}_"))
                    && e.path().extension().and_then(|s| s.to_str()) == Some("ts")
            });
        assert!(has_ts, "{stem} 应有对应 ts 段");
    }

    println!(
        "[ffmpeg_real] ABR 多档位转码成功：master={} 字节，目录文件数={}",
        master.len(),
        std::fs::read_dir(&dir).unwrap().count()
    );
    println!("[ffmpeg_real] master playlist 内容：\n{master}");
}

// ----------------------------------------------------------------------------
// 真实测（附带）：master playlist 内容正确性（纯函数，用真实 build_master_playlist）
// ----------------------------------------------------------------------------

/// 用真实 `build_master_playlist` 构造三档位 master，断言 BANDWIDTH/RESOLUTION 正确。
/// 此测不带 `#[ignore]`（纯函数，无外部依赖），但作为真实测配套验证。
#[tokio::test]
async fn real_master_playlist_content_correct() {
    let variants = vec![
        HlsVariant::from_profile(TranscodeProfile::Hls1080p),
        HlsVariant::from_profile(TranscodeProfile::Hls720p),
        HlsVariant::from_profile(TranscodeProfile::Hls480p),
    ];
    let master = build_master_playlist(&variants);
    assert!(master.starts_with("#EXTM3U\n"));
    assert!(master.contains("#EXT-X-VERSION:3\n"));
    // 三档位带宽（bps）。
    assert!(master.contains("BANDWIDTH=5000000"), "1080p 带宽");
    assert!(master.contains("BANDWIDTH=2800000"), "720p 带宽");
    assert!(master.contains("BANDWIDTH=1400000"), "480p 带宽");
    // 三档位分辨率（16:9）。
    assert!(master.contains("RESOLUTION=1920x1080"));
    assert!(master.contains("RESOLUTION=1280x720"));
    // 480p：480 × 16/9 = 853.33 → round = 853。
    assert!(master.contains("RESOLUTION=853x480"));
    println!("[ffmpeg_real] master playlist 内容校验通过");
}
