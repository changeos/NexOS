//! PXE 引导产物真实测：iPXE/pxelinux.cfg 模板生成验证 + dnsmasq PXE 配置语法校验。
//!
//! 本文件是 os-provision 的集成测试（`tests/`，对 crate 黑盒；不进 lib 单元测计数）。
//!
//! 设计意图（provision-pxe-real 任务 DoD）：
//! - **A. 模板生成验证（默认跑，纯逻辑）**：覆盖 [`PxeConfigBuilder::build`] 产物的
//!   结构正确性——iPXE bootstrap.ipxe 脚本结构（shebang + dhcp + kernel + initrd +
//!   boot + http repo URL）、pxelinux.cfg/default 模板（DEFAULT/PROMPT/TIMEOUT/LABEL +
//!   KERNEL）、UEFI/BIOS bootfile 选型、TFTP 文件清单完整性、DHCP 摘要（next-server +
//!   boot_filename）。
//! - **B. dnsmasq PXE 配置语法校验（`#[ignore]`，需本机 dnsmasq）**：把
//!   [`PxeBootParams`] 翻译成真实 dnsmasq 配置片段（`dhcp-range` + `enable-tftp` +
//!   `tftp-root` + `dhcp-boot=<bootfile>,<next-server>` + `pxe-service=<CSA>,...,<basename>`），
//!   跑 `dnsmasq --test --conf-file=<tmpfile>`（`--test` **只校验语法不真启服务**，
//!   不影响宿主 DHCP）验证零语法错误。
//!
//! 关键设计点：
//! - 不改 trait 签名、不真启 dnsmasq 服务（只 `--test` 语法校验）、不碰宿主 DHCP。
//! - 临时配置 + `/tmp` 目录，测完 RAII 清理（[`TftprootGuard`] + [`TempConfGuard`]）。
//! - 优雅 SKIP：无 dnsmasq 时提前 `return`（不 panic），保持 `--ignored` 套件可重复运行。
//! - 三种 BootMode（BIOS/UEFI/UEFIArm64）映射 dnsmasq CSA 标签（`x86PC`/`x86-64_EFI`/
//!   `ARM64_EFI`，见 dnsmasq manpage `--pxe-service`）。
//!
//! 运行：
//! ```bash
//! # A. 模板生成测（默认跑）
//! cargo test -p os-provision --features mock --test pxe_real
//! # B. dnsmasq 真实测（需本机装 dnsmasq：sudo apt install -y dnsmasq）
//! cargo test -p os-provision --features mock --test pxe_real -- --ignored --nocapture
//! ```

use std::fs;
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::Command;

use os_provision::pxe::{BootMode, PxeBootParams, PxeConfigBuilder};

// ============================================================================
// 辅助构造
// ============================================================================

/// 样例 PXE 参数（与 src/pxe.rs 单测、provision_pxe_bootstrap.rs 集成测一致的 fixture）。
fn sample_params(boot_mode: BootMode) -> PxeBootParams {
    PxeBootParams {
        http_repo: "http://10.0.0.1:8080/provision".into(),
        kernel_path: "vmlinuz".into(),
        initramfs_path: "initrd.img".into(),
        base_image_path: "base.squashfs".into(),
        install_disk: "/dev/sda".into(),
        tftp_server: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
        boot_mode,
    }
}

/// 纯 Rust 的 `which`：扫 `$PATH` 找可执行文件（不引 which crate，呼应 runc_real.rs）。
fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 探针：返回 dnsmasq 路径，缺则 eprintln + None（不 panic）。
fn require_dnsmasq() -> Option<PathBuf> {
    // 优先 $PATH，回退常见路径（dnsmasq 常装在 /usr/sbin）。
    if let Some(p) = which("dnsmasq") {
        return Some(p);
    }
    for candidate in [
        "/usr/sbin/dnsmasq",
        "/usr/bin/dnsmasq",
        "/usr/local/sbin/dnsmasq",
    ] {
        if Path::new(candidate).is_file() {
            return Some(PathBuf::from(candidate));
        }
    }
    eprintln!(
        "[SKIP] 未找到 dnsmasq（PATH + /usr/sbin 等均无）。\
         装：sudo apt install -y dnsmasq；\
         重跑：cargo test -p os-provision --features mock --test pxe_real -- --ignored --nocapture"
    );
    None
}

/// BootMode → dnsmasq `--pxe-service` 的 CSA（Client System Architecture）标签。
///
/// 见 dnsmasq manpage `--pxe-service`：已知类型含
/// `x86PC` / `IA32_EFI` / `x86-64_EFI` / `Xscale_EFI` / `BC_EFI` / `ARM32_EFI` / `ARM64_EFI`。
fn csa_label(boot_mode: BootMode) -> &'static str {
    match boot_mode {
        BootMode::Bios => "x86PC",
        BootMode::Uefi => "x86-64_EFI",
        BootMode::UefiArm64 => "ARM64_EFI",
    }
}

/// 把 [`PxeBootParams`] 翻译成 dnsmasq PXE 配置片段（仅语法校验用，不真启服务）。
///
/// 字段映射：
/// - `dhcp-range`：固定假网段（仅语法占位，不真分配；`--test` 不下 DHCP）。
/// - `enable-tftp` + `tftp-root`：开启 dnsmasq 内建 TFTP，根目录指向 `tftproot`。
/// - `dhcp-boot=<bootfile>,<next-server>`：DHCP option 67/66，喂自 [`PxeArtifacts::dhcp`]。
/// - `pxe-service=<CSA>,<menu>,<basename>`：按架构选 NBP basename。
/// - `port=0`：关闭 DNS 端口（避免占 53 影响宿主 named/systemd-resolved）。
fn build_dnsmasq_config(params: &PxeBootParams, tftproot: &Path) -> String {
    let artifacts = PxeConfigBuilder::build(params);
    let bootfile = artifacts.dhcp.boot_filename.as_str();
    let next_server = artifacts.dhcp.next_server.as_str();
    let csa = csa_label(params.boot_mode);

    // pxe-service basename：dnsmasq 会自动加 ".0" layer 后缀（除非 basename 自带后缀）。
    // iPXE 二进制（ipxe.efi 等）自带后缀 → 传完整名；pxelinux.0 → 去掉 ".0" 传 "pxelinux"。
    // （manpage：「Alternatively, the basename may be a filename, complete with suffix,
    //   in which case no layer suffix is added.」）
    let pxe_basename = if bootfile.ends_with(".efi") || bootfile.ends_with(".kpxe") {
        // iPXE/iPXE-kpxe：完整文件名（含后缀），dnsmasq 不再加 ".0"。
        bootfile.to_string()
    } else {
        // pxelinux.0：去掉 ".0"，dnsmasq 自动补 layer 后缀。
        bootfile.trim_end_matches(".0").to_string()
    };

    format!(
        "# 由 os-provision pxe_real 测自动生成——dnsmasq --test 语法校验用\n\
         port=0\n\
         dhcp-range=10.0.0.100,10.0.0.200,12h\n\
         enable-tftp\n\
         tftp-root={tftproot}\n\
         dhcp-boot={bootfile},{next_server}\n\
         pxe-service={csa},\"OS PXE Boot\",{pxe_basename}\n",
        tftproot = tftproot.display(),
        bootfile = bootfile,
        next_server = next_server,
        csa = csa,
        pxe_basename = pxe_basename,
    )
}

/// 生成本次测独占的临时路径（基于进程 id + 计数器，避免并发测互相踩）。
///
/// 不引入 tempfile 依赖（os-provision 主依赖保持精简，呼应 iso/storage real 测做法）。
fn unique_tmp(label: &str) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("os-provision-pxe-real-{label}-{pid}-{n}"))
}

// ============================================================================
// RAII 清理 guard
// ============================================================================

/// tftp-root 目录 guard：Drop 时递归删除（含子目录 pxelinux.cfg/）。
struct TftprootGuard {
    path: PathBuf,
    armed: bool,
}

impl TftprootGuard {
    fn new(label: &str) -> Self {
        let path = unique_tmp(label);
        let _ = fs::create_dir_all(&path);
        Self { path, armed: true }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// 测主体成功后调用，避免 Drop 重复清理（虽然 rm 不存在目录不报错，disarm 更语义化）。
    #[allow(dead_code)]
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TftprootGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// 临时配置文件 guard：Drop 时删文件。
struct TempConfGuard {
    path: PathBuf,
    armed: bool,
}

impl TempConfGuard {
    /// 把内容写到 `<tmpdir>/<label>.conf`，返回 guard。
    fn write(label: &str, content: &str) -> std::io::Result<Self> {
        let path = unique_tmp(label).with_extension("conf");
        fs::write(&path, content)?;
        Ok(Self { path, armed: true })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempConfGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

// ============================================================================
// A. 模板生成验证测（默认跑，纯逻辑）
// ============================================================================

#[test]
fn a1_ipxe_bootstrap_script_structure() {
    // iPXE bootstrap.ipxe 脚本结构正确：含 shebang + kernel + initrd + boot + http repo URL。
    let p = sample_params(BootMode::Uefi);
    let artifacts = PxeConfigBuilder::build(&p);
    let ipxe = artifacts
        .find_file("bootstrap.ipxe")
        .expect("bootstrap.ipxe must exist in artifacts.files");

    let s = &ipxe.content;
    // 1. shebang（iPXE 解释器声明，必须有）
    assert!(
        s.starts_with("#!ipxe\n"),
        "iPXE 脚本必须以 #!ipxe\\n 开头（实际首行: {:?})",
        s.lines().next().unwrap_or("")
    );
    // 2. kernel 行：拉取 vmlinuz + cmdline（base_image + install_disk）
    assert!(
        s.contains("kernel http://10.0.0.1:8080/provision/vmlinuz"),
        "kernel 行必须含完整 http repo URL"
    );
    assert!(
        s.contains("base_image=http://10.0.0.1:8080/provision/base.squashfs"),
        "kernel cmdline 必须含 base_image=<完整 URL>"
    );
    assert!(
        s.contains("install_disk=/dev/sda"),
        "kernel cmdline 必须含 install_disk=<目标盘>"
    );
    // 3. initrd 行：拉取 initramfs
    assert!(
        s.contains("initrd http://10.0.0.1:8080/provision/initrd.img"),
        "initrd 行必须含完整 http repo URL"
    );
    // 4. boot 指令（触发引导，必须末尾）
    assert!(
        s.ends_with("boot\n"),
        "iPXE 脚本必须以 boot\\n 结尾（实际末尾: {:?})",
        s.chars()
            .rev()
            .take(20)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>()
    );
}

#[test]
fn a2_pxelinux_default_template_structure() {
    // pxelinux.cfg/default 模板结构：DEFAULT/PROMPT/TIMEOUT/LABEL + KERNEL。
    let p = sample_params(BootMode::Bios);
    let artifacts = PxeConfigBuilder::build(&p);
    let cfg = artifacts
        .find_file("pxelinux.cfg/default")
        .expect("pxelinux.cfg/default must exist");
    let s = &cfg.content;

    assert!(s.contains("DEFAULT ipxe"), "必须有 DEFAULT 指令");
    assert!(s.contains("PROMPT 0"), "必须有 PROMPT 指令");
    assert!(s.contains("TIMEOUT 10"), "必须有 TIMEOUT 指令");
    assert!(s.contains("LABEL ipxe"), "必须有 LABEL 块");
    // KERNEL 指令指向 iPXE 二进制（BIOS → undionly.kpxe）
    assert!(
        s.contains("KERNEL undionly.kpxe"),
        "BIOS 模式 KERNEL 必须指向 undionly.kpxe（链式加载 iPXE）"
    );
}

#[test]
fn a3_uefi_bootfile_is_ipxe_efi() {
    // UEFI boot mode：DHCP bootfile = ipxe.efi（直接 iPXE，不走 pxelinux）。
    let p = sample_params(BootMode::Uefi);
    let artifacts = PxeConfigBuilder::build(&p);
    assert_eq!(
        artifacts.dhcp.boot_filename, "ipxe.efi",
        "UEFI 模式 DHCP boot_filename 必须是 ipxe.efi"
    );
    // pxelinux.cfg/default 即便生成，KERNEL 也指向 ipxe.efi（UEFI fallback 备用）
    let cfg = artifacts.find_file("pxelinux.cfg/default").unwrap();
    assert!(
        cfg.content.contains("KERNEL ipxe.efi"),
        "UEFI pxelinux.cfg/default KERNEL 应指向 ipxe.efi"
    );
}

#[test]
fn a4_bios_bootfile_is_pxelinux_chain_loads_undionly() {
    // BIOS boot mode：bootfile = pxelinux.0，TFTP 清单含 pxelinux.0 + ldlinux.c32 + undionly.kpxe。
    let p = sample_params(BootMode::Bios);
    let artifacts = PxeConfigBuilder::build(&p);
    assert_eq!(
        artifacts.dhcp.boot_filename, "pxelinux.0",
        "BIOS 模式 DHCP boot_filename 必须是 pxelinux.0"
    );

    let names: Vec<&str> = artifacts
        .tftp_manifest
        .iter()
        .map(|e| e.rel_path.as_str())
        .collect();
    // BIOS 链：DHCP → pxelinux.0 → KERNEL undionly.kpxe（iPXE）→ bootstrap.ipxe → kernel/initrd
    assert!(
        names.contains(&"pxelinux.0"),
        "BIOS TFTP 清单必须含 pxelinux.0（DHCP NBP）"
    );
    assert!(
        names.contains(&"undionly.kpxe"),
        "BIOS TFTP 清单必须含 undionly.kpxe（pxelinux 链式加载的 iPXE 二进制）"
    );
    assert!(
        names.contains(&"ldlinux.c32"),
        "BIOS TFTP 清单必须含 ldlinux.c32（pxelinux.0 运行时依赖模块）"
    );
}

#[test]
fn a5_tftp_manifest_complete_all_modes() {
    // TFTP 文件清单完整：覆盖三种 BootMode 各自需要的二进制 + 文本文件。
    for (mode, expect_ipxe_bin, expect_pxelinux) in [
        (BootMode::Uefi, "ipxe.efi", false),
        (BootMode::Bios, "undionly.kpxe", true),
        (BootMode::UefiArm64, "ipxe-arm64.efi", false),
    ] {
        let p = sample_params(mode);
        let artifacts = PxeConfigBuilder::build(&p);
        let names: Vec<&str> = artifacts
            .tftp_manifest
            .iter()
            .map(|e| e.rel_path.as_str())
            .collect();

        // 通用：必含 iPXE 二进制 + bootstrap.ipxe（iPXE 脚本文本）
        assert!(
            names.contains(&expect_ipxe_bin),
            "BootMode {:?} TFTP 清单必须含 iPXE 二进制 {}",
            mode,
            expect_ipxe_bin
        );
        assert!(
            names.contains(&"bootstrap.ipxe"),
            "BootMode {:?} TFTP 清单必须含 bootstrap.ipxe（iPXE 脚本）",
            mode
        );

        if expect_pxelinux {
            assert!(
                names.contains(&"pxelinux.0"),
                "BIOS 模式 TFTP 清单必须含 pxelinux.0"
            );
        } else {
            assert!(
                !names.contains(&"pxelinux.0"),
                "UEFI/ARM64 模式不应含 pxelinux.0（DHCP 直接指向 ipxe.efi/ipxe-arm64.efi）"
            );
        }
    }
}

#[test]
fn a6_dhcp_summary_has_next_server_and_bootfile() {
    // DHCP 摘要含 next-server（option 66）+ boot_filename（option 67），与 PxeBootParams 对齐。
    let p = sample_params(BootMode::Uefi);
    let artifacts = PxeConfigBuilder::build(&p);

    assert_eq!(artifacts.dhcp.boot_filename, "ipxe.efi");
    // next_server 必须等于 PxeBootParams.tftp_server 的字符串形式（不含 CIDR/前缀）
    assert_eq!(artifacts.dhcp.next_server, "10.0.0.1");
    // 反序列化回 IpAddr 应等于原始 tftp_server（验证可被 set_boot_file 解析）
    let parsed: IpAddr = artifacts
        .dhcp
        .next_server
        .parse()
        .expect("next_server 必须是合法 IpAddr 字符串");
    assert_eq!(parsed, p.tftp_server);
}

// ============================================================================
// B. dnsmasq PXE 配置真实验证测（#[ignore]，需本机 dnsmasq）
// ============================================================================

/// B1. dnsmasq 可达性：`dnsmasq --version` 与空配置 `--test` 均 exit 0。
///
/// 不需 root（--test/--version 无特权需求），但标 `#[ignore]` 因依赖本机 dnsmasq 装机。
#[test]
#[ignore = "真实 dnsmasq：需本机 dnsmasq 二进制，人工 `cargo test -- --ignored`"]
fn b1_dnsmasq_reachable_version_and_empty_test() {
    let bin = match require_dnsmasq() {
        Some(p) => p,
        None => return,
    };

    // 1. --version：exit 0，stdout 含 "Dnsmasq version"
    let out = Command::new(&bin).arg("--version").output();
    let out = match out {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[SKIP] 调 dnsmasq --version 失败: {e}");
            return;
        }
    };
    assert!(
        out.status.success(),
        "dnsmasq --version 应 exit 0（实际 {:?}），stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Dnsmasq") && stdout.contains("version"),
        "dnsmasq --version stdout 应含版本标识，实际: {}",
        stdout
    );
    eprintln!(
        "[OK] dnsmasq 版本: {}",
        stdout.lines().next().unwrap_or("(空)")
    );

    // 2. 空配置 --test：exit 0，stderr 含 "syntax check OK"
    let guard = TempConfGuard::write("b1_empty", "# empty config\n").expect("写空配置");
    // NOTE: dnsmasq 的 --conf-file 必须用 `=` 连接值（不能空格分隔，否则报
    // "junk found in command line"）；等价短选项 -C 可空格分隔。这里用 `=` 形式。
    let out = Command::new(&bin)
        .arg("--test")
        .arg(format!("--conf-file={}", guard.path().display()))
        .output()
        .expect("调 dnsmasq --test");
    assert!(
        out.status.success(),
        "空配置 --test 应 exit 0（实际 {:?}），stderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("syntax check OK"),
        "空配置 --test stderr 应含 'syntax check OK'，实际: {}",
        stderr
    );
    eprintln!("[OK] 空配置 --test 通过");
}

/// B2. dnsmasq PXE 配置语法校验：根据 PxeBootParams 构造 dnsmasq 配置片段，
/// `--test` 验证零语法错误。覆盖三种 BootMode。
#[test]
#[ignore = "真实 dnsmasq：PXE 配置语法校验，需本机 dnsmasq"]
fn b2_dnsmasq_pxe_config_syntax_all_boot_modes() {
    let bin = match require_dnsmasq() {
        Some(p) => p,
        None => return,
    };

    for mode in [BootMode::Bios, BootMode::Uefi, BootMode::UefiArm64] {
        let _tftproot = TftprootGuard::new(&format!("b2-{:?}", mode));
        let p = sample_params(mode);
        let config = build_dnsmasq_config(&p, _tftproot.path());

        eprintln!(
            "[{}] dnsmasq 配置片段:\n----\n{}----",
            mode_label(mode),
            config
        );

        let guard =
            TempConfGuard::write(&format!("b2-{:?}", mode), &config).expect("写 dnsmasq PXE 配置");
        // dnsmasq --conf-file 必须用 `=` 连接（见 b1 注释）。
        let out = Command::new(&bin)
            .arg("--test")
            .arg(format!("--conf-file={}", guard.path().display()))
            .output()
            .expect("调 dnsmasq --test");

        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "BootMode {:?} dnsmasq 配置应通过 --test（实际 {:?}），stderr: {}\n配置:\n{}",
            mode,
            out.status.code(),
            stderr,
            config
        );
        assert!(
            stderr.contains("syntax check OK"),
            "BootMode {:?} stderr 应含 'syntax check OK'，实际: {}",
            mode,
            stderr
        );
        eprintln!("[OK] BootMode {:?} dnsmasq PXE 配置语法校验通过", mode);
    }
}

/// B3. tftp-root 目录结构验证：把 PxeArtifacts 的文本文件写到 /tmp 临时 tftp-root，
/// 验证文件布局路径正确（dnsmasq 能识别的目录结构），并跑 `--test` 确认 tftp-root 路径合法。
///
/// 仅文件存在性 + 路径正确，不真启 TFTP 服务（`--test` 不开端口）。
#[test]
#[ignore = "真实 dnsmasq：tftp-root 目录结构验证，需本机 dnsmasq"]
fn b3_tftp_root_layout_recognized_by_dnsmasq() {
    let bin = match require_dnsmasq() {
        Some(p) => p,
        None => return,
    };

    let tftproot = TftprootGuard::new("b3");

    // 把 PxeArtifacts 的文本文件（bootstrap.ipxe + pxelinux.cfg/default）落到 tftp-root
    let p = sample_params(BootMode::Bios);
    let artifacts = PxeConfigBuilder::build(&p);
    for file in &artifacts.files {
        let dest = tftproot.path().join(&file.rel_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).expect("建子目录（如 pxelinux.cfg/）");
        }
        fs::write(&dest, &file.content).expect("写引导文件到 tftp-root");
        assert!(
            dest.exists(),
            "tftp-root 内文件必须落盘成功: {}",
            dest.display()
        );
    }

    // 验证布局：bootstrap.ipxe + pxelinux.cfg/default 都在
    let bootstrap = tftproot.path().join("bootstrap.ipxe");
    let pxelinux_cfg = tftproot.path().join("pxelinux.cfg").join("default");
    assert!(bootstrap.is_file(), "tftp-root/bootstrap.ipxe 必须存在");
    assert!(
        pxelinux_cfg.is_file(),
        "tftp-root/pxelinux.cfg/default 必须存在"
    );

    // 文件内容非空（落盘完整）
    let bs_content = fs::read_to_string(&bootstrap).unwrap();
    assert!(
        bs_content.starts_with("#!ipxe"),
        "落盘的 bootstrap.ipxe 内容应正确"
    );
    let cfg_content = fs::read_to_string(&pxelinux_cfg).unwrap();
    assert!(cfg_content.contains("DEFAULT ipxe"));

    // 跑 dnsmasq --test 验证 tftp-root 路径合法（指向存在的目录）
    let config = build_dnsmasq_config(&p, tftproot.path());
    let guard = TempConfGuard::write("b3-layout", &config).expect("写 b3 配置");
    // dnsmasq --conf-file 必须用 `=` 连接（见 b1 注释）。
    let out = Command::new(&bin)
        .arg("--test")
        .arg(format!("--conf-file={}", guard.path().display()))
        .output()
        .expect("调 dnsmasq --test");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "tftp-root 布局 + 配置应通过 --test（实际 {:?}），stderr: {}",
        out.status.code(),
        stderr
    );
    eprintln!(
        "[OK] tftp-root 目录布局（{}）被 dnsmasq 配置语法识别",
        tftproot.path().display()
    );
    eprintln!(
        "     布局: bootstrap.ipxe ({}, {} 字节), pxelinux.cfg/default ({}, {} 字节)",
        bootstrap.display(),
        bs_content.len(),
        pxelinux_cfg.display(),
        cfg_content.len()
    );
}

/// BootMode → 人读标签（日志用）。
fn mode_label(m: BootMode) -> &'static str {
    match m {
        BootMode::Bios => "BIOS",
        BootMode::Uefi => "UEFI",
        BootMode::UefiArm64 => "UEFI-ARM64",
    }
}
