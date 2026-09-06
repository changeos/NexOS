//! ISO 完整构建端到端测（`#[ignore]`——需 `apt install xorriso squashfs-tools`）。
//!
//! 本文件是 os-iso 的集成测试（`tests/`，对 crate 黑盒；不进 lib 单元测计数）。
//!
//! 定位（与 [`real_xorriso_build.rs`] 互补）：
//! - [`real_xorriso_build.rs`] 验证「xorriso ISO 构建能力 + 4 种启动模式（BIOS-only /
//!   BIOS+UEFI / UEFI-only / builder-e2e）」。
//! - 本文件补一个**完整链路测**：组装 rootfs → mksquashfs → xorriso → ISO 产物
//!   **深度验证**（文件结构 / sha256 / 大小），并新增 rootfs squashfs 往返测
//!   （mksquashfs → unsquashfs → 文件还原校验）。
//!
//! 三组测：
//! - **A. 完整 ISO 构建测** ([`full_iso_build_and_verify`])：rootfs 组装 →
//!   mksquashfs → xorriso → 断言 ISO 存在/非空/`file(1)` 识别 ISO9660/
//!   `xorriso -ls /` 验证结构/sha256 非空/大小一致/RAII 清理。
//! - **B. rootfs squashfs 往返测** ([`rootfs_squashfs_roundtrip`])：rootfs →
//!   mksquashfs → unsquashfs → 验证所有文件内容/权限/结构完整还原。
//! - **C. installer 命令构造验证** ([`installer_cmd_construction`])：installer-impl
//!   子代理的纯函数 `partition_cmd` / `create_pool_cmd` 当前**未落地**
//!   （impl_installer.rs 无此类命令构造函数），故此测以 SKIPPED 占位形式登记，
//!   待 installer 命令构造 API 落地后填充——避免无意义 panic。
//!
//! 全部 `#[ignore]`；无 xorriso/mksquashfs 时 [`require_iso_tools`] 优雅 SKIP
//! （打印明确跳过原因，不留无意义栈）。
//!
//! 运行：
//! ```bash
//! sudo apt install -y xorriso squashfs-tools
//! cargo test -p os-iso --features mock --test iso_full_build_real -- --ignored --nocapture
//! ```

#![cfg(test)]

use os_iso::env::IsoEnvironment;
use os_iso::runner::{IsoBuildRunner, TokioIsoRunner};
use std::path::{Path, PathBuf};

// ----------------------------------------------------------------------------
// ISO9660 魔数常量（与 real_xorriso_build.rs 一致，本测独立定义避免跨文件耦合）
// ----------------------------------------------------------------------------

/// ISO9660 Volume Descriptor 标识符：`CD001`。
const ISO9660_MAGIC: &[u8] = b"CD001";
/// Volume Descriptor 起始偏移（逻辑扇区 16 × 2048）。
const ISO9660_VD_OFFSET: usize = 0x8000;
/// `CD001` 标识符偏移（VD 起始 + 1 字节 VD Type）。
const ISO9660_VD_ID_OFFSET: usize = 0x8001;
/// squashfs 魔数（小端 `hsqs`）。
const SQUASHFS_MAGIC: &[u8] = b"hsqs";

// ----------------------------------------------------------------------------
// 临时目录 / fixture / 工具函数
// ----------------------------------------------------------------------------

/// 生成本次测独占的临时工作目录（进程 id + 计数器，避免并发测互相踩）。
///
/// 与 `real_xorriso_build.rs::unique_workdir` 同策略：不引入 tempfile 依赖，
/// 测结束手动清理（RAII 风格——见各测末尾 `remove_dir_all`，以及
/// [`WorkdirGuard`] 的 drop 兜底）。
fn unique_workdir(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("os-iso-full-{label}-{pid}-{n}"))
}

/// RAII guard：drop 时清理临时工作目录，即使断言 panic 也不留垃圾。
///
/// 即使测体内已显式 `remove_dir_all`（早清理），guard 的 drop 仍兜底
/// （对已删目录的 remove_dir_all 错误被静默吞掉）。
struct WorkdirGuard(PathBuf);
impl Drop for WorkdirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 组装一个**完整 rootfs fixture**：模拟真实 OS rootfs 的 binary / config / 目录结构。
///
/// 形如：
/// ```text
/// <rootfs>/
///   etc/hostname              "os-e2e"
///   etc/os-release            "NAME=\"OS E2E\"\nVERSION=1.0\n"
///   etc/os/config.json       {"host":"os-e2e","version":"1.0"}
///   usr/bin/osd              #!/bin/sh ... （模拟 binary）
///   usr/bin/os-storage       #!/bin/sh ... （模拟 binary）
///   opt/os/version           "1.0.0-e2e"
///   README.md                 "# OS E2E rootfs fixture"
/// ```
///
/// 文件内容刻意可区分（含特征字符串），便于 B 部分往返测逐文件比对。
fn assemble_rootfs(rootfs: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(rootfs.join("etc/os"))?;
    std::fs::create_dir_all(rootfs.join("usr/bin"))?;
    std::fs::create_dir_all(rootfs.join("opt/os"))?;

    std::fs::write(rootfs.join("etc/hostname"), "os-e2e\n")?;
    std::fs::write(
        rootfs.join("etc/os-release"),
        "NAME=\"OS E2E\"\nVERSION=\"1.0\"\nID=os-e2e\n",
    )?;
    std::fs::write(
        rootfs.join("etc/os/config.json"),
        "{\"host\":\"os-e2e\",\"version\":\"1.0\",\"components\":[\"osd\",\"os-storage\"]}\n",
    )?;
    // 模拟 binary（带可执行位，便于 B 部分验证权限还原）
    let osd = rootfs.join("usr/bin/osd");
    std::fs::write(&osd, "#!/bin/sh\n# osd stub binary (e2e)\nexit 0\n")?;
    set_exec(&osd)?;
    let os_storage = rootfs.join("usr/bin/os-storage");
    std::fs::write(
        &os_storage,
        "#!/bin/sh\n# os-storage stub binary (e2e)\nexit 0\n",
    )?;
    set_exec(&os_storage)?;
    std::fs::write(rootfs.join("opt/os/version"), "1.0.0-e2e\n")?;
    std::fs::write(rootfs.join("README.md"), "# OS E2E rootfs fixture\n")?;
    Ok(())
}

/// 设置文件可执行位（Unix；非 Unix 静默忽略）。
fn set_exec(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms)?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// 取文件可执行位是否设置（Unix；非 Unix 返回 false）。
#[cfg(unix)]
fn is_exec(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
#[cfg(not(unix))]
#[allow(dead_code)]
fn is_exec(_path: &Path) -> bool {
    false
}

/// 校验 ISO9660 魔数（与 real_xorriso_build.rs 同形态）。
fn assert_iso9660_magic(iso_path: &Path) {
    let meta = std::fs::metadata(iso_path)
        .unwrap_or_else(|e| panic!("ISO 产物不存在 {}: {e}", iso_path.display()));
    assert!(meta.len() > 0, "ISO 产物为空：{}", iso_path.display());
    assert!(
        meta.len() as usize >= ISO9660_VD_ID_OFFSET + ISO9660_MAGIC.len(),
        "ISO 太小（{} 字节），不可能是合法 ISO9660",
        meta.len()
    );
    let bytes = std::fs::read(iso_path)
        .unwrap_or_else(|e| panic!("读 ISO 失败 {}: {e}", iso_path.display()));
    let vd_id = &bytes[ISO9660_VD_ID_OFFSET..ISO9660_VD_ID_OFFSET + ISO9660_MAGIC.len()];
    assert_eq!(
        vd_id,
        ISO9660_MAGIC,
        "ISO9660 魔数校验失败：VD ID 偏移 {:#x} 处期望 {:?} 实得 {:?}",
        ISO9660_VD_ID_OFFSET,
        std::str::from_utf8(ISO9660_MAGIC).unwrap_or("?"),
        std::str::from_utf8(vd_id).unwrap_or("?"),
    );
    let vd_type = bytes[ISO9660_VD_OFFSET];
    assert!(
        matches!(vd_type, 0 | 1 | 2 | 3 | 255),
        "VD type 异常：{vd_type}（应为 0/1/2/3/255）"
    );
}

/// 调 `file(1)` 校验 ISO 被识别为 ISO 9660（缺 file 命令则跳过断言）。
fn assert_file_reports_iso9660(iso_path: &Path) {
    let out = std::process::Command::new("file")
        .arg("-b")
        .arg(iso_path)
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            eprintln!("[file] {} → {}", iso_path.display(), s.trim());
            assert!(
                s.contains("ISO 9660") || s.contains("iso9660"),
                "file(1) 未识别为 ISO 9660：{s}"
            );
        }
        Ok(o) => eprintln!(
            "[file] 调用非零退出（{}），跳过 ISO 9660 断言：{}",
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => eprintln!("[file] 缺失 file(1)：{e}（跳过 ISO 9660 断言）"),
    }
}

/// 用 xorriso `-ls /` 列出 ISO 根目录内容，断言 squashfs 文件在 ISO 中可见。
async fn assert_xorriso_lists_structure(iso_path: &Path) {
    let runner = TokioIsoRunner::new();
    // 列根目录
    let out = runner
        .run(
            "xorriso",
            &[
                "-indev".to_string(),
                iso_path.to_string_lossy().into_owned(),
                "-ls".to_string(),
                "/".to_string(),
            ],
        )
        .await
        .expect("xorriso -ls / spawn 失败");
    assert!(out.is_success(), "xorriso -ls / 失败: {}", out.stderr);
    let root_listing = format!("{}\n{}", out.stdout, out.stderr);
    eprintln!("[xorriso -ls /]\n{}", root_listing.trim_end());
    assert!(
        root_listing.contains("casper"),
        "xorriso -ls / 未列出 'casper' 目录，输出：{root_listing}"
    );

    // 列 /casper（squashfs 容器）
    let out2 = runner
        .run(
            "xorriso",
            &[
                "-indev".to_string(),
                iso_path.to_string_lossy().into_owned(),
                "-ls".to_string(),
                "/casper".to_string(),
            ],
        )
        .await
        .expect("xorriso -ls /casper spawn 失败");
    let casper_listing = format!("{}\n{}", out2.stdout, out2.stderr);
    eprintln!("[xorriso -ls /casper]\n{}", casper_listing.trim_end());
    assert!(
        casper_listing.contains("filesystem.squashfs"),
        "xorriso -ls /casper 未列出 filesystem.squashfs，输出：{casper_listing}"
    );
}

/// 校验 squashfs 产物：存在 + 非空 + `hsqs` 魔数。
fn assert_squashfs_magic(sqfs_path: &Path) {
    let meta = std::fs::metadata(sqfs_path)
        .unwrap_or_else(|e| panic!("squashfs 产物不存在 {}: {e}", sqfs_path.display()));
    assert!(meta.len() > 0, "squashfs 产物为空：{}", sqfs_path.display());
    let bytes = std::fs::read(sqfs_path).unwrap_or_else(|e| panic!("读 squashfs 失败: {e}"));
    assert!(
        bytes.len() >= SQUASHFS_MAGIC.len(),
        "squashfs 太小（{} 字节），不含魔数",
        bytes.len()
    );
    assert_eq!(
        &bytes[..SQUASHFS_MAGIC.len()],
        SQUASHFS_MAGIC,
        "squashfs 魔数校验失败：期望 {:?} 实得 {:?}",
        std::str::from_utf8(SQUASHFS_MAGIC).unwrap_or("?"),
        std::str::from_utf8(&bytes[..SQUASHFS_MAGIC.len()]).unwrap_or("?"),
    );
}

/// 缺 ISO 工具时统一优雅 SKIP（打印明确原因，不 panic 出栈）。
///
/// 返回 `Option<IsoEnvironment>`：`Some(env)` = 工具齐全可继续；
/// `None` = 应跳过（已 eprintln 跳过原因）。
fn probe_iso_tools() -> Option<IsoEnvironment> {
    let env = IsoEnvironment::probe();
    if !env.is_capable() {
        let missing = env.missing_tools().join(", ");
        eprintln!(
            "[SKIP] ISO 完整构建测：缺少工具 [{missing}]。\n\
             安装：sudo apt install -y xorriso squashfs-tools\n\
             重跑：cargo test -p os-iso --features mock --test iso_full_build_real -- --ignored --nocapture"
        );
        return None;
    }
    Some(env)
}

// ============================================================================
// A. 完整 ISO 构建测（rootfs → mksquashfs → xorriso → 产物深度验证）
// ============================================================================

/// 完整链路：组装 rootfs → mksquashfs → xorriso → ISO 产物深度验证。
///
/// 验证项：
/// 1. squashfs 产物存在 + 非空 + `hsqs` 魔数。
/// 2. ISO 产物存在 + 非空 + ISO9660 `CD001` 魔数。
/// 3. `file(1)` 识别 ISO 9660。
/// 4. `xorriso -ls /` 与 `-ls /casper` 列出预期结构（casper/filesystem.squashfs）。
/// 5. sha256 非空 + 64 位 hex；两次计算一致（确定性）。
/// 6. `TokioIsoRunner::file_size` 与 `std::fs::metadata` 一致。
/// 7. RAII 清理（`WorkdirGuard` drop 兜底）。
#[tokio::test]
#[ignore = "需 xorriso + squashfs-tools：sudo apt install -y xorriso squashfs-tools"]
async fn full_iso_build_and_verify() {
    let env = match probe_iso_tools() {
        Some(e) => e,
        None => return,
    };
    assert!(env.has_xorriso && env.has_mksquashfs);

    let work = unique_workdir("full");
    let _guard = WorkdirGuard(work.clone()); // RAII 兜底清理

    let rootfs = work.join("rootfs");
    let iso_tree = work.join("iso-tree");
    let casper_dir = iso_tree.join("casper");
    let squashfs_path = casper_dir.join("filesystem.squashfs");
    let iso_path = work.join("os-e2e.iso");

    std::fs::create_dir_all(&rootfs).unwrap();
    std::fs::create_dir_all(&casper_dir).unwrap();
    assemble_rootfs(&rootfs).expect("组装 rootfs fixture 失败");

    let runner = TokioIsoRunner::new();

    // —— 阶段一：mksquashfs（rootfs → filesystem.squashfs）——
    let sq_args = vec![
        rootfs.to_string_lossy().into_owned(),
        squashfs_path.to_string_lossy().into_owned(),
        "-noappend".to_string(),
        "-comp".to_string(),
        "xz".to_string(),
        "-b".to_string(),
        "1048576".to_string(),
    ];
    let sq_out = runner
        .run("mksquashfs", &sq_args)
        .await
        .expect("mksquashfs spawn 失败");
    assert!(
        sq_out.is_success(),
        "mksquashfs 失败 (exit {}): {}",
        sq_out.exit_code,
        sq_out.stderr
    );
    assert_squashfs_magic(&squashfs_path);
    let sqfs_size = std::fs::metadata(&squashfs_path).map(|m| m.len()).unwrap();
    eprintln!(
        "[A] mksquashfs OK: {} ({} 字节)",
        squashfs_path.display(),
        sqfs_size
    );

    // ISO 源树额外放一个 README（证明 squashfs 之外的内容也能进 ISO）
    std::fs::write(iso_tree.join("README.md"), "# OS E2E ISO\n").unwrap();

    // —— 阶段二：xorriso 生成 ISO（数据 ISO，不带 El Torito 引导——聚焦产物验证）——
    let xo_args = vec![
        "-as".to_string(),
        "mkisofs".to_string(),
        "-r".to_string(),
        "-V".to_string(),
        "OS-E2E".to_string(),
        "-J".to_string(),
        "-joliet-long".to_string(),
        "-o".to_string(),
        iso_path.to_string_lossy().into_owned(),
        iso_tree.to_string_lossy().into_owned(),
    ];
    let xo_out = runner
        .run("xorriso", &xo_args)
        .await
        .expect("xorriso spawn 失败");
    assert!(
        xo_out.is_success(),
        "xorriso 失败 (exit {}): stdout={}\nstderr={}",
        xo_out.exit_code,
        xo_out.stdout,
        xo_out.stderr
    );

    // —— 阶段三：产物深度验证 ——
    assert_iso9660_magic(&iso_path);
    assert_file_reports_iso9660(&iso_path);
    assert_xorriso_lists_structure(&iso_path).await;

    // sha256：非空 + 64 位 hex + 两次计算一致
    let hash1 = runner
        .compute_sha256(&iso_path)
        .await
        .expect("compute_sha256 (1) 失败");
    let hash2 = runner
        .compute_sha256(&iso_path)
        .await
        .expect("compute_sha256 (2) 失败");
    assert!(!hash1.is_empty(), "sha256 不应为空");
    assert_eq!(hash1.len(), 64, "sha256 长度应为 64，实际 {}", hash1.len());
    assert!(
        hash1.chars().all(|c| c.is_ascii_hexdigit()),
        "sha256 含非 hex 字符: {hash1}"
    );
    assert_eq!(hash1, hash2, "sha256 两次计算不一致（不确定性）");

    // 大小：runner.file_size 与 metadata 一致
    let runner_size = runner.file_size(&iso_path).await.expect("file_size 失败");
    let meta_size = std::fs::metadata(&iso_path).map(|m| m.len()).unwrap();
    assert_eq!(
        runner_size, meta_size,
        "TokioIsoRunner::file_size 与元数据不一致"
    );
    assert!(
        meta_size > sqfs_size,
        "ISO 应大于内嵌 squashfs（{meta_size} > {sqfs_size}）"
    );

    eprintln!(
        "[A] 完整 ISO 构建成功：{} ({} 字节, sha256={}...)",
        iso_path.display(),
        meta_size,
        &hash1[..12]
    );
    eprintln!(
        "[A] 测完成，临时目录将由 WorkdirGuard drop 清理：{}",
        work.display()
    );
}

// ============================================================================
// B. rootfs squashfs 往返测（mksquashfs → unsquashfs → 文件还原校验）
// ============================================================================

/// rootfs → mksquashfs → unsquashfs → 验证所有文件内容/权限/结构完整还原。
///
/// 验证项：
/// 1. mksquashfs 产物 `hsqs` 魔数。
/// 2. unsquashfs 还原后目录结构与原 rootfs 一致（etc / usr/bin / opt/os / README.md）。
/// 3. 文件**内容**逐字节一致（含特征字符串）。
/// 4. 模拟 binary（usr/bin/osd, usr/bin/os-storage）可执行位保留（Unix）。
/// 5. 两次 unsquashfs（不同输出目录）结果一致（确定性）。
#[tokio::test]
#[ignore = "需 mksquashfs + unsquashfs：sudo apt install -y squashfs-tools"]
async fn rootfs_squashfs_roundtrip() {
    let env = match probe_iso_tools() {
        Some(e) => e,
        None => return,
    };
    // unsquashfs 也属 squashfs-tools 包，与 mksquashfs 同进退；额外探一次以稳。
    if !which_exists("unsquashfs") {
        eprintln!("[SKIP] squashfs 往返测：缺 unsquashfs（apt install squashfs-tools）");
        return;
    }
    assert!(env.has_mksquashfs);

    let work = unique_workdir("rt");
    let _guard = WorkdirGuard(work.clone());

    let rootfs = work.join("rootfs");
    let sqfs = work.join("filesystem.squashfs");
    let unpack1 = work.join("unpack1");
    let unpack2 = work.join("unpack2");

    std::fs::create_dir_all(&rootfs).unwrap();
    assemble_rootfs(&rootfs).expect("组装 rootfs fixture 失败");

    let runner = TokioIsoRunner::new();

    // —— mksquashfs ——
    let mk_args = vec![
        rootfs.to_string_lossy().into_owned(),
        sqfs.to_string_lossy().into_owned(),
        "-noappend".to_string(),
        "-comp".to_string(),
        "xz".to_string(),
        "-b".to_string(),
        "1048576".to_string(),
    ];
    let mk_out = runner
        .run("mksquashfs", &mk_args)
        .await
        .expect("mksquashfs spawn 失败");
    assert!(mk_out.is_success(), "mksquashfs 失败: {}", mk_out.stderr);
    assert_squashfs_magic(&sqfs);

    // —— unsquashfs（两次，不同输出目录）——
    for unpack in [&unpack1, &unpack2] {
        // unsquashfs 要求目标目录不存在或为空；传 -f 强制，-d 指定输出。
        let un_args = vec![
            "-f".to_string(),
            "-d".to_string(),
            unpack.to_string_lossy().into_owned(),
            sqfs.to_string_lossy().into_owned(),
        ];
        let un_out = runner
            .run("unsquashfs", &un_args)
            .await
            .expect("unsquashfs spawn 失败");
        assert!(
            un_out.is_success(),
            "unsquashfs 失败 (exit {}): {}",
            un_out.exit_code,
            un_out.stderr
        );
    }

    // —— 文件还原校验（逐文件内容 + 权限）——
    let expected_files: &[(&str, &[u8])] = &[
        ("etc/hostname", b"os-e2e\n"),
        (
            "etc/os-release",
            b"NAME=\"OS E2E\"\nVERSION=\"1.0\"\nID=os-e2e\n",
        ),
        (
            "etc/os/config.json",
            b"{\"host\":\"os-e2e\",\"version\":\"1.0\",\"components\":[\"osd\",\"os-storage\"]}\n",
        ),
        (
            "usr/bin/osd",
            b"#!/bin/sh\n# osd stub binary (e2e)\nexit 0\n",
        ),
        (
            "usr/bin/os-storage",
            b"#!/bin/sh\n# os-storage stub binary (e2e)\nexit 0\n",
        ),
        ("opt/os/version", b"1.0.0-e2e\n"),
        ("README.md", b"# OS E2E rootfs fixture\n"),
    ];

    for unpack in [&unpack1, &unpack2] {
        for (rel, expect_content) in expected_files {
            let restored = unpack.join(rel);
            let meta = std::fs::metadata(&restored)
                .unwrap_or_else(|e| panic!("还原文件缺失 {}: {e}", restored.display()));
            assert!(meta.is_file(), "还原路径非文件：{}", restored.display());
            let got = std::fs::read(&restored)
                .unwrap_or_else(|e| panic!("读还原文件失败 {}: {e}", restored.display()));
            assert_eq!(
                got,
                *expect_content,
                "文件内容不一致 {}: 期望 {:?} 实得 {:?}",
                restored.display(),
                std::str::from_utf8(expect_content).unwrap_or("?"),
                std::str::from_utf8(&got).unwrap_or("?"),
            );
        }
        // 可执行位保留（Unix）：模拟 binary
        #[cfg(unix)]
        {
            assert!(is_exec(&unpack.join("usr/bin/osd")), "osd 可执行位丢失");
            assert!(
                is_exec(&unpack.join("usr/bin/os-storage")),
                "os-storage 可执行位丢失"
            );
        }
    }

    // 两次 unsquashfs 结果一致：对每个文件做 hash 比对（用文件大小+内容相等已隐含）
    let perm_note = if cfg!(unix) {
        " + 可执行位保留"
    } else {
        ""
    };
    eprintln!(
        "[B] rootfs squashfs 往返成功：{} → unsquashfs → 7 文件全部还原（内容一致{perm_note}）",
        sqfs.display(),
    );
}

// ============================================================================
// C. installer 命令构造验证（占位——API 未落地则 SKIP）
// ============================================================================

/// installer-impl 子代理的纯命令构造函数（`partition_cmd` / `create_pool_cmd`）
/// 验证占位。
///
/// **现状**：`crates/os-iso/src/impl_installer.rs` 与 `installer.rs` 当前**无**
/// `partition_cmd` / `create_pool_cmd` 这类命令构造纯函数（installer 仅有
/// `RustInstaller` 的 HCL 检测 + 占位 disk 接口，命令派生未落地）。
///
/// 本测以 SKIPPED 形式登记此缺口，待 installer 命令构造 API 落地后替换为真实
/// argv 断言（如 `assert!(partition_cmd(...).contains(&"--part-type".to_string()))`）。
/// 现在不做无意义 panic——这是 e2e 测文件，留空跑通编译即可。
#[tokio::test]
#[ignore = "installer 命令构造 API（partition_cmd/create_pool_cmd）尚未落地"]
async fn installer_cmd_construction() {
    eprintln!(
        "[SKIP] installer 命令构造验证：partition_cmd / create_pool_cmd \
         当前未在 os-iso::impl_installer 中实现。待 installer-impl 子代理落地 \
         命令派生纯函数后填充本测。"
    );
    // 不 panic——优雅 SKIP（与 require_iso_tools 缺工具时行为一致）。
}

// ----------------------------------------------------------------------------
// 小工具
// ----------------------------------------------------------------------------

/// 探测程序是否在 `$PATH`（仅本测 B 部分用——unsquashfs 与 mksquashfs 同包，
/// 但额外探一次以稳）。复用 `IsoEnvironment::Probe` 会更 DRY，但 `Probe` 是
/// `pub` 的，这里直接复用以避免重复实现。
fn which_exists(program: &str) -> bool {
    os_iso::env::Probe::exists(program)
}
