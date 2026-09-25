//! xorriso / mksquashfs CLI 命令构造——纯函数，可单元测，不执行真实子进程。
//!
//! 设计动机（呼应 os-storage `cli.rs` 的同款做法）：`XorrisoIsoBuilder` 的核心可测逻辑
//! 是「构造正确的 xorriso / mksquashfs 命令参数」。把命令构造抽成纯函数（返回
//! `Vec<String>` 参数列表），就能在不 spawn 子进程的前提下用断言验证 CLI 形态，避免依赖
//! 真实 xorriso/squashfs 工具链（开发机通常无此工具，规格书 §6 要求沙箱）。
//!
//! 命名：`squashfs_*` / `xorriso_*` 对应 CLI 工具名；返回 `Vec<String>` 是「程序名之后的参数」
//! （调用方在此基础上 `Command::new("mksquashfs").args(...)`）。
//!
//! 真实执行（spawn 子进程）留 `XorrisoIsoBuilder::build` 内 TODO（需沙箱 + 工具链）。

use crate::iso::IsoSpec;

// ----------------------------------------------------------------------------
// squashfs 打包（rootfs → squashfs.img）
// ----------------------------------------------------------------------------

/// squashfs 压缩配置（呼应规格书 §3：可配算法与块大小）。
#[derive(Debug, Clone)]
pub struct SquashfsConfig {
    /// 源 rootfs 目录（被打包的目录树）。
    pub source_dir: String,
    /// 产物 squashfs 文件路径。
    pub output_file: String,
    /// 压缩算法（`gzip` / `xz` / `zstd`；默认 `xz`，呼应 Ubuntu live-boot 默认）。
    pub comp: String,
    /// 块大小（字节；默认 1 MiB = 1048576）。
    pub block_size: u32,
}

impl SquashfsConfig {
    /// 默认配置（xz + 1 MiB 块），给定源目录与输出文件。
    #[must_use]
    pub fn new(source_dir: impl Into<String>, output_file: impl Into<String>) -> Self {
        Self {
            source_dir: source_dir.into(),
            output_file: output_file.into(),
            comp: "xz".to_string(),
            block_size: 1_048_576,
        }
    }

    /// 设置压缩算法。
    #[must_use]
    pub fn with_comp(mut self, comp: impl Into<String>) -> Self {
        self.comp = comp.into();
        self
    }

    /// 设置块大小（字节）。
    #[must_use]
    pub fn with_block_size(mut self, block_size: u32) -> Self {
        self.block_size = block_size;
        self
    }
}

/// 构造 `mksquashfs <source> <output> -comp <algo> -b <block>` 的参数列表。
///
/// 注：`mksquashfs` 无 `-noappend` 时会追加到既有产物，此处显式 `-noappend` 保证幂等。
/// 真实执行需 mksquashfs 二进制（squashfs-tools 包）。
pub(crate) fn squashfs_pack_args(cfg: &SquashfsConfig) -> Vec<String> {
    vec![
        cfg.source_dir.clone(),
        cfg.output_file.clone(),
        "-noappend".to_string(),
        "-comp".to_string(),
        cfg.comp.clone(),
        "-b".to_string(),
        cfg.block_size.to_string(),
    ]
}

// ----------------------------------------------------------------------------
// xorriso 生成 ISO
// ----------------------------------------------------------------------------

/// El Torito 启动配置（呼应规格书 §3：BIOS/UEFI 可启动 ISO）。
#[derive(Debug, Clone)]
pub struct BootConfig {
    /// 引导镜像（boot image）相对 ISO 根的路径，如 `/boot/grub/i386-pc/eltorito.img`。
    pub boot_image: String,
    /// ISO 卷标（volume id），如 `OS-ISO`。
    pub volume_id: String,
    /// 是否启用 UEFI 启动（追加 `-efi-boot` / `efi_boot_partition`）。
    pub efi: bool,
    /// EFI 引导镜像路径（`efi=true` 时使用），如 `/boot/efi.img`。
    pub efi_boot_image: Option<String>,
}

impl BootConfig {
    /// BIOS + UEFI 双启配置（默认）。
    #[must_use]
    pub fn new(volume_id: impl Into<String>, boot_image: impl Into<String>) -> Self {
        Self {
            boot_image: boot_image.into(),
            volume_id: volume_id.into(),
            efi: true,
            efi_boot_image: Some("/boot/efi.img".to_string()),
        }
    }

    /// 仅 BIOS 启动（无 UEFI）。
    #[must_use]
    pub fn bios_only(mut self) -> Self {
        self.efi = false;
        self.efi_boot_image = None;
        self
    }
}

/// 构造 xorriso 命令参数（生成可启动 ISO）。
///
/// xorriso 调用形态（`-as mkisofs` 兼容模式，最通用）：
/// ```text
/// xorriso -as mkisofs \
///   -r -V <volume_id> \
///   -J -joliet-long \
///   -b <boot_image> -boot-info-table -boot-load-size 4 -no-emul-boot \
///   [-eltorito-alt-boot -e <efi_image> -no-emul-boot] \
///   -o <output_iso> <source_tree>
/// ```
///
/// 注：BIOS 引导信息表选项必须写作 `-boot-info-table`（xorriso 1.5.x `-as mkisofs`
/// 兼容模式的标准 mkisofs 选项名）；写作 `-boot-info` 会被 xorriso 以
/// `Unrecognized option '-boot-info'` 拒收（已真实测验证）。
///
/// - `cfg`：启动配置（卷标 / 引导镜像 / UEFI 开关）
/// - `source_tree`：ISO 根目录（已含 rootfs.squashfs + components + 引导文件）
/// - `output_iso`：产物 ISO 路径
pub(crate) fn xorriso_build_args(
    cfg: &BootConfig,
    source_tree: &str,
    output_iso: &str,
) -> Vec<String> {
    let mut args = vec![
        "-as".to_string(),
        "mkisofs".to_string(),
        "-r".to_string(),
        "-V".to_string(),
        cfg.volume_id.clone(),
        "-J".to_string(),
        "-joliet-long".to_string(),
        // El Torito BIOS 引导项（-boot-info-table 是 xorriso -as mkisofs 的正确选项名）
        "-b".to_string(),
        cfg.boot_image.clone(),
        "-boot-info-table".to_string(),
        "-boot-load-size".to_string(),
        "4".to_string(),
        "-no-emul-boot".to_string(),
    ];
    // El Torito UEFI 备用引导项（若启用）
    if cfg.efi {
        if let Some(efi_img) = &cfg.efi_boot_image {
            args.push("-eltorito-alt-boot".to_string());
            args.push("-e".to_string());
            args.push(efi_img.clone());
            args.push("-no-emul-boot".to_string());
        }
    }
    args.push("-o".to_string());
    args.push(output_iso.to_string());
    args.push(source_tree.to_string());
    args
}

// ----------------------------------------------------------------------------
// sha256 校验
// ----------------------------------------------------------------------------

/// 构造 `sha256sum <file>` 的参数列表（用于 verify 步骤）。
#[allow(dead_code)]
pub(crate) fn sha256sum_args(file: &str) -> Vec<String> {
    vec![file.to_string()]
}

/// 从 `sha256sum` 标准输出解析出 hex 摘要（小写 64 位）。
///
/// 输出形如 `<hash>  <file>\n`；取首个空白前字段。失败返回 None（由调用方转 IsoError）。
///
/// 当前为骨架阶段——真实 `verify` 的 sha256sum 子进程执行留 TODO，此解析函数待
/// 真实现调用，故 allow dead_code（已有单测覆盖解析正确性）。
#[allow(dead_code)]
pub(crate) fn parse_sha256sum_output(out: &str) -> Option<String> {
    let first_token = out.split_whitespace().next()?;
    if first_token.len() == 64 && first_token.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(first_token.to_ascii_lowercase())
    } else {
        None
    }
}

// ----------------------------------------------------------------------------
// IsoSpec → 构建参数派生
// ----------------------------------------------------------------------------

/// 由 `IsoSpec` 推导默认启动配置（卷标包含变体与 ubuntu 版本，便于辨识）。
///
/// 卷标规则：`OS-<variant>-<ubuntu_version>`（如 `OS-clone-24.04`），受 ISO 9660 卷标
/// 长度限制（≤ 32 字节），过长截断。
pub(crate) fn derive_boot_config(spec: &IsoSpec) -> BootConfig {
    let variant_tag = match &spec.variant {
        crate::iso::IsoVariant::Standard => "std",
        crate::iso::IsoVariant::Clone { .. } => "clone",
    };
    let mut vol = format!("OS-{}-{}", variant_tag, spec.ubuntu_version);
    if vol.len() > 32 {
        vol.truncate(32);
    }
    BootConfig::new(vol, "/boot/grub/i386-pc/eltorito.img")
}

// ----------------------------------------------------------------------------
// 单元测试（纯函数，无工具链依赖）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iso::{IsoSpec, IsoVariant};

    #[test]
    fn squashfs_config_defaults() {
        let cfg = SquashfsConfig::new("/src", "/out/fs.squashfs");
        assert_eq!(cfg.comp, "xz");
        assert_eq!(cfg.block_size, 1_048_576);
        assert_eq!(cfg.source_dir, "/src");
        assert_eq!(cfg.output_file, "/out/fs.squashfs");
    }

    #[test]
    fn squashfs_config_overrides() {
        let cfg = SquashfsConfig::new("/src", "/out/x")
            .with_comp("zstd")
            .with_block_size(262_144);
        assert_eq!(cfg.comp, "zstd");
        assert_eq!(cfg.block_size, 262_144);
    }

    #[test]
    fn squashfs_pack_args_shape() {
        let cfg = SquashfsConfig::new("/src", "/out/fs.squashfs");
        let args = squashfs_pack_args(&cfg);
        assert_eq!(args[0], "/src");
        assert_eq!(args[1], "/out/fs.squashfs");
        assert!(args.contains(&"-noappend".to_string()));
        assert!(args.contains(&"-comp".to_string()));
        assert!(args.contains(&"xz".to_string()));
        assert!(args.contains(&"-b".to_string()));
        assert!(args.contains(&"1048576".to_string()));
    }

    #[test]
    fn boot_config_defaults_bios_uefi() {
        let cfg = BootConfig::new("OS-ISO", "/boot/img.bin");
        assert!(cfg.efi);
        assert_eq!(cfg.efi_boot_image.as_deref(), Some("/boot/efi.img"));
    }

    #[test]
    fn boot_config_bios_only() {
        let cfg = BootConfig::new("OS-ISO", "/boot/img.bin").bios_only();
        assert!(!cfg.efi);
        assert!(cfg.efi_boot_image.is_none());
    }

    #[test]
    fn xorriso_build_args_bios_uefi() {
        let cfg = BootConfig::new("OS-STD-24.04", "/boot/img.bin");
        let args = xorriso_build_args(&cfg, "/tree", "/out.iso");
        assert!(args.contains(&"-as".to_string()));
        assert!(args.contains(&"mkisofs".to_string()));
        assert!(args.contains(&"-V".to_string()));
        assert!(args.contains(&"OS-STD-24.04".to_string()));
        // BIOS 引导项
        assert!(args.contains(&"-b".to_string()));
        assert!(args.contains(&"/boot/img.bin".to_string()));
        assert!(args.contains(&"-boot-info-table".to_string()));
        assert!(args.contains(&"-no-emul-boot".to_string()));
        // UEFI 备用引导
        assert!(args.contains(&"-eltorito-alt-boot".to_string()));
        assert!(args.contains(&"-e".to_string()));
        assert!(args.contains(&"/boot/efi.img".to_string()));
        // 输出
        assert!(args.contains(&"-o".to_string()));
        assert!(args.contains(&"/out.iso".to_string()));
        assert!(args.contains(&"/tree".to_string()));
    }

    #[test]
    fn xorriso_build_args_bios_only_no_efi() {
        let cfg = BootConfig::new("OS", "/boot/img.bin").bios_only();
        let args = xorriso_build_args(&cfg, "/tree", "/out.iso");
        assert!(!args.contains(&"-eltorito-alt-boot".to_string()));
        assert!(!args.contains(&"/boot/efi.img".to_string()));
    }

    #[test]
    fn sha256sum_args_shape() {
        let args = sha256sum_args("/tmp/x.iso");
        assert_eq!(args, vec!["/tmp/x.iso".to_string()]);
    }

    #[test]
    fn parse_sha256sum_output_ok() {
        let out = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789  /tmp/x.iso\n";
        let parsed = parse_sha256sum_output(out);
        assert_eq!(
            parsed,
            Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string())
        );
    }

    #[test]
    fn parse_sha256sum_output_uppercase() {
        let out = "ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789ABCDEF0123456789  f\n";
        assert_eq!(
            parse_sha256sum_output(out),
            Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".to_string())
        );
    }

    #[test]
    fn parse_sha256sum_output_invalid() {
        assert_eq!(parse_sha256sum_output("short  f\n"), None);
        assert_eq!(parse_sha256sum_output(""), None);
        assert_eq!(parse_sha256sum_output("zzzz...  f\n"), None);
    }

    #[test]
    fn derive_boot_config_volume_id_within_limit() {
        let spec = IsoSpec {
            variant: IsoVariant::Standard,
            base_image: "x".into(),
            components: vec!["osd".into()],
            ubuntu_version: "24.04".to_string(),
            arch: "x86_64".into(),
            locale: "zh_CN.UTF-8".into(),
        };
        let cfg = derive_boot_config(&spec);
        assert!(cfg.volume_id.len() <= 32);
        assert!(cfg.volume_id.starts_with("OS-"));
        assert!(cfg.volume_id.contains("24.04"));
    }

    #[test]
    fn derive_boot_config_clone_tag() {
        let spec = IsoSpec {
            variant: IsoVariant::Clone {
                config_snapshot: serde_json::json!({}),
            },
            base_image: "x".into(),
            components: vec!["osd".into()],
            ubuntu_version: "24.04".to_string(),
            arch: "aarch64".into(),
            locale: "en_US.UTF-8".into(),
        };
        let cfg = derive_boot_config(&spec);
        assert!(cfg.volume_id.contains("clone"));
    }

    #[test]
    fn derive_boot_config_truncates_long_version() {
        let spec = IsoSpec {
            variant: IsoVariant::Standard,
            base_image: "x".into(),
            components: vec!["osd".into()],
            ubuntu_version: "24.04.1-LTS-with-very-long-suffix-string".to_string(),
            arch: "x86_64".into(),
            locale: "zh_CN.UTF-8".into(),
        };
        let cfg = derive_boot_config(&spec);
        assert!(cfg.volume_id.len() <= 32);
    }
}
