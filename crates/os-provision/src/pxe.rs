//! PXE 引导配置生成（规划文档 §3.10 阶段1 / §3.9 PXE）
//!
//! 纯逻辑模块：给定自举目标 + 基础镜像等参数，生成 PXE 引导所需的**配置产物**
//! （文本字符串），交由下游（`os-network::PxeServer` 设置 DHCP next-server/bootfile，
//! TFTP 服务落盘）。本 crate 不真跑 PXE/DHCP/TFTP（红线：不真分区建池、不修改 trait）。
//!
//! 生成三类产物：
//! 1. **iPXE 引导脚本**（`bootstrap.ipxe`）——从 HTTP 拉取内核 + initramfs + base 镜像，
//!    进入阶段1 安装环境（initramfs 跑 [`crate::init_script`] 的安装脚本）。
//! 2. **pxelinux.cfg/default**（`pxelinux` 链式加载 iPXE 的 fallback 路径）——
//!    老网卡不支持 iPXE 时，BIOS PXE → pxelinux → 加载 iPXE（undionly.kpxe）。
//! 3. **DHCP next-server + bootfile 配置**（与 `PxeServer::set_boot_file` 形参对齐，
//!    调用方据此调 `PxeServer`）。
//! 4. **TFTP 文件布局清单**——应放进 tftp root 的文件清单（路径 + 推荐来源）。
//!
//! 设计：所有方法纯函数、可单测；输出文本用 `String`（含 `'\n'`）便于 diff 比对。

use std::net::IpAddr;

use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// PXE 引导参数
// ----------------------------------------------------------------------------

/// PXE 引导配置参数（生成引导产物所需）。
///
/// 与 [`crate::provision::ProvisionTarget`] / [`crate::provision::ProvisionConfig`]
/// 配合：前者描述"装到哪台机"，本结构描述"PXE 引导细节参数"（HTTP 仓库、内核路径等）。
/// 调用方（`PxeProvisioner`）把两者组合喂给 [`PxeConfigBuilder`]。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PxeBootParams {
    /// HTTP 仓库基地址（iPXE 从此拉取内核/initramfs/base 镜像）。
    /// 例：`http://10.0.0.1:8080/provision`
    pub http_repo: String,
    /// 内核相对路径（相对 `http_repo`）。例：`vmlinuz`
    pub kernel_path: String,
    /// initramfs 相对路径。例：`initrd.img`
    pub initramfs_path: String,
    /// base 镜像相对路径（squashfs/rootfs，阶段1 安装源）。例：`base.squashfs`
    pub base_image_path: String,
    /// 目标节点的根磁盘（安装目标，传给 initramfs）。例：`/dev/sda`
    pub install_disk: String,
    /// TFTP 服务器地址（DHCP next-server；通常本机）。
    pub tftp_server: IpAddr,
    /// 引导模式（UEFI / BIOS）——决定 bootfile 与 pxelinux 模板分支。
    pub boot_mode: BootMode,
}

/// 引导模式（决定 bootfile 选 iPXE UEFI / BIOS NBP）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BootMode {
    /// BIOS 传统引导（bootfile = `pxelinux.0`，链式加载 undionly.kpxe）
    Bios,
    /// UEFI 引导（bootfile = `ipxe.efi`，直接 iPXE）
    Uefi,
    /// UEFI ARM64（bootfile = `ipxe-arm64.efi`）
    UefiArm64,
}

impl BootMode {
    /// 默认 bootfile 名（DHCP option 67）。
    pub fn default_bootfile(self) -> &'static str {
        match self {
            BootMode::Bios => "pxelinux.0",
            BootMode::Uefi => "ipxe.efi",
            BootMode::UefiArm64 => "ipxe-arm64.efi",
        }
    }

    /// 默认 iPXE 二进制（TFTP 内文件名）。
    pub fn default_ipxe_binary(self) -> &'static str {
        match self {
            BootMode::Bios => "undionly.kpxe",
            BootMode::Uefi => "ipxe.efi",
            BootMode::UefiArm64 => "ipxe-arm64.efi",
        }
    }
}

// ----------------------------------------------------------------------------
// PXE 引导产物
// ----------------------------------------------------------------------------

/// 一份 PXE 引导文件（在 TFTP root 内的相对路径 + 文本内容）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PxeFile {
    /// TFTP root 内的相对路径（POSIX 风格，正斜杠）。例：`pxelinux.cfg/default`
    pub rel_path: String,
    /// 文件文本内容（UTF-8）。
    pub content: String,
}

/// 一项 TFTP 文件布局清单（应放进 tftp root 的文件）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TftpManifestEntry {
    /// TFTP root 内相对路径。例：`undionly.kpxe`
    pub rel_path: String,
    /// 来源说明（人读，标注从哪获取/是否二进制等）。例：`ipxe.org 编译产物（二进制）`
    pub source: String,
    /// 是否二进制（true 则下游不能当文本读）。
    pub is_binary: bool,
}

/// DHCP 配置摘要（喂给 [`os_network::services::PxeServer::set_boot_file`]）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DhcpPxeConfig {
    /// bootfile（DHCP option 67）。
    pub boot_filename: String,
    /// next-server（DHCP option 66，TFTP 服务器 IP 字符串形式）。
    pub next_server: String,
}

// ----------------------------------------------------------------------------
// 配置生成器（纯函数）
// ----------------------------------------------------------------------------

/// PXE 引导配置生成器——纯逻辑，把 [`PxeBootParams`] 转为可落盘的引导产物。
///
/// 不依赖 `PxeServer`/网络/文件系统——纯字符串生成，便于 fixture 测覆盖所有分支。
/// 下游编排器（`PxeProvisioner`）拿到 [`PxeArtifacts`] 后：
/// 1. 调 `PxeServer::set_boot_file(dhcp.boot_filename, dhcp.next_server.parse().unwrap())`；
/// 2. 把 `files` 落到 TFTP root；
/// 3. 按 `tftp_manifest` 准备 iPXE/pxelinux 二进制。
#[derive(Debug, Clone, Default)]
pub struct PxeConfigBuilder;

impl PxeConfigBuilder {
    /// 生成全部 PXE 引导产物（iPXE 脚本 + pxelinux.cfg/default + DHCP 摘要 + TFTP 清单）。
    pub fn build(params: &PxeBootParams) -> PxeArtifacts {
        let ipxe_script = Self::build_ipxe_script(params);
        let pxelinux_default = Self::build_pxelinux_default(params);
        let dhcp = DhcpPxeConfig {
            boot_filename: params.boot_mode.default_bootfile().to_string(),
            next_server: params.tftp_server.to_string(),
        };

        // 引导文件：iPXE 脚本 + pxelinux.cfg/default
        let files = vec![
            PxeFile {
                rel_path: "bootstrap.ipxe".into(),
                content: ipxe_script,
            },
            PxeFile {
                rel_path: "pxelinux.cfg/default".into(),
                content: pxelinux_default,
            },
        ];

        // TFTP 清单：iPXE 二进制 + pxelinux + lpxelinux（BIOS 路径）
        let mut tftp_manifest = Vec::new();
        tftp_manifest.push(TftpManifestEntry {
            rel_path: params.boot_mode.default_ipxe_binary().into(),
            source: "ipxe.org 编译产物（二进制 NBP）".into(),
            is_binary: true,
        });
        if matches!(params.boot_mode, BootMode::Bios) {
            tftp_manifest.push(TftpManifestEntry {
                rel_path: "pxelinux.0".into(),
                source: "syslinux 包（二进制 NBP，BIOS fallback）".into(),
                is_binary: true,
            });
            tftp_manifest.push(TftpManifestEntry {
                rel_path: "ldlinux.c32".into(),
                source: "syslinux 模块（pxelinux.0 依赖）".into(),
                is_binary: true,
            });
        }
        tftp_manifest.push(TftpManifestEntry {
            rel_path: "bootstrap.ipxe".into(),
            source: "本生成器产出的 iPXE 脚本（文本）".into(),
            is_binary: false,
        });

        PxeArtifacts {
            files,
            dhcp,
            tftp_manifest,
        }
    }

    /// 生成 iPXE 引导脚本——从 HTTP 仓库拉取内核 + initramfs + base 镜像，
    /// 并把安装目标盘与 base 镜像路径作为 cmdline 传给内核（initramfs 解析后跑安装脚本）。
    pub fn build_ipxe_script(params: &PxeBootParams) -> String {
        let kernel_url = join_url(&params.http_repo, &params.kernel_path);
        let initrd_url = join_url(&params.http_repo, &params.initramfs_path);
        let base_url = join_url(&params.http_repo, &params.base_image_path);

        let mut s = String::new();
        s.push_str("#!ipxe\n");
        s.push_str("# 由 os-provision::pxe 自动生成——勿手改\n");
        s.push_str("# iPXE 引导脚本：拉取内核 + initramfs + base 镜像，进入阶段1 安装环境\n");
        s.push_str(&format!(
            "echo [os-provision] 从 {} 引导\n",
            params.http_repo
        ));
        s.push_str(&format!(
            "kernel {} base_image={} install_disk={}\n",
            kernel_url, base_url, params.install_disk
        ));
        s.push_str(&format!("initrd {}\n", initrd_url));
        s.push_str("boot\n");
        s
    }

    /// 生成 pxelinux.cfg/default——BIOS PXE 链式加载 iPXE。
    /// UEFI 模式下不需要此文件，但本生成器仍产出（调用方可按 boot_mode 选择性落盘）。
    pub fn build_pxelinux_default(params: &PxeBootParams) -> String {
        let ipxe_binary = params.boot_mode.default_ipxe_binary();
        let mut s = String::new();
        s.push_str("# 由 os-provision::pxe 自动生成——勿手改\n");
        s.push_str("# pxelinux.cfg/default：BIOS PXE 链式加载 iPXE\n");
        s.push_str("DEFAULT ipxe\n");
        s.push_str("PROMPT 0\n");
        s.push_str("TIMEOUT 10\n");
        s.push('\n');
        s.push_str("LABEL ipxe\n");
        s.push_str(&format!("  KERNEL {}\n", ipxe_binary));
        s.push('\n');
        s.push_str("# 注：UEFI 模式下 DHCP 直接指向 ipxe.efi，本文件不生效\n");
        s
    }
}

/// PXE 引导产物集合（生成器输出）。
#[derive(Debug, Clone)]
pub struct PxeArtifacts {
    /// 引导文件（iPXE 脚本 + pxelinux.cfg/default）。
    pub files: Vec<PxeFile>,
    /// DHCP 配置摘要（喂给 `PxeServer::set_boot_file`）。
    pub dhcp: DhcpPxeConfig,
    /// TFTP 文件布局清单（应放进 tftp root 的二进制 + 文本文件）。
    pub tftp_manifest: Vec<TftpManifestEntry>,
}

impl PxeArtifacts {
    /// 按 TFTP 相对路径查找引导文件。
    pub fn find_file(&self, rel_path: &str) -> Option<&PxeFile> {
        self.files.iter().find(|f| f.rel_path == rel_path)
    }
}

// ----------------------------------------------------------------------------
// 内部工具
// ----------------------------------------------------------------------------

/// 拼接 URL（base 末尾去多余 `/`，path 前补 `/`）。
fn join_url(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let path = path.trim_start_matches('/');
    if path.is_empty() {
        base.to_string()
    } else {
        format!("{}/{}", base, path)
    }
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

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

    #[test]
    fn boot_mode_bootfiles() {
        assert_eq!(BootMode::Bios.default_bootfile(), "pxelinux.0");
        assert_eq!(BootMode::Uefi.default_bootfile(), "ipxe.efi");
        assert_eq!(BootMode::UefiArm64.default_bootfile(), "ipxe-arm64.efi");
    }

    #[test]
    fn join_url_handles_trailing_slash() {
        assert_eq!(join_url("http://a/", "b"), "http://a/b");
        assert_eq!(join_url("http://a", "b"), "http://a/b");
        assert_eq!(join_url("http://a/", ""), "http://a");
        assert_eq!(join_url("http://a", "/b/c"), "http://a/b/c");
    }

    #[test]
    fn ipxe_script_contains_required_lines() {
        let p = sample_params(BootMode::Uefi);
        let s = PxeConfigBuilder::build_ipxe_script(&p);
        assert!(s.starts_with("#!ipxe\n"));
        assert!(s.contains("kernel http://10.0.0.1:8080/provision/vmlinuz"));
        assert!(s.contains("base_image=http://10.0.0.1:8080/provision/base.squashfs"));
        assert!(s.contains("install_disk=/dev/sda"));
        assert!(s.contains("initrd http://10.0.0.1:8080/provision/initrd.img"));
        assert!(s.ends_with("boot\n"));
    }

    #[test]
    fn pxelinux_default_has_label() {
        let p = sample_params(BootMode::Bios);
        let s = PxeConfigBuilder::build_pxelinux_default(&p);
        assert!(s.contains("DEFAULT ipxe"));
        assert!(s.contains("KERNEL undionly.kpxe"));
    }

    #[test]
    fn build_artifacts_uefi() {
        let p = sample_params(BootMode::Uefi);
        let a = PxeConfigBuilder::build(&p);

        // DHCP 摘要
        assert_eq!(a.dhcp.boot_filename, "ipxe.efi");
        assert_eq!(a.dhcp.next_server, "10.0.0.1");

        // 引导文件
        assert_eq!(a.files.len(), 2);
        let f = a.find_file("bootstrap.ipxe").expect("有 bootstrap.ipxe");
        assert!(f.content.starts_with("#!ipxe\n"));
        let cfg = a
            .find_file("pxelinux.cfg/default")
            .expect("有 pxelinux.cfg/default");
        assert!(cfg.content.contains("DEFAULT ipxe"));

        // TFTP 清单：UEFI 下无 pxelinux.0/ldlinux.c32
        let names: Vec<&str> = a
            .tftp_manifest
            .iter()
            .map(|m| m.rel_path.as_str())
            .collect();
        assert!(names.contains(&"ipxe.efi"));
        assert!(!names.contains(&"pxelinux.0"));
        assert!(!names.contains(&"ldlinux.c32"));
    }

    #[test]
    fn build_artifacts_bios_has_pxelinux() {
        let p = sample_params(BootMode::Bios);
        let a = PxeConfigBuilder::build(&p);

        assert_eq!(a.dhcp.boot_filename, "pxelinux.0");
        let names: Vec<&str> = a
            .tftp_manifest
            .iter()
            .map(|m| m.rel_path.as_str())
            .collect();
        // BIOS 路径需 pxelinux.0 + ldlinux.c32 + undionly.kpxe
        assert!(names.contains(&"undionly.kpxe"));
        assert!(names.contains(&"pxelinux.0"));
        assert!(names.contains(&"ldlinux.c32"));
    }

    #[test]
    fn build_artifacts_arm64() {
        let p = sample_params(BootMode::UefiArm64);
        let a = PxeConfigBuilder::build(&p);
        assert_eq!(a.dhcp.boot_filename, "ipxe-arm64.efi");
        let names: Vec<&str> = a
            .tftp_manifest
            .iter()
            .map(|m| m.rel_path.as_str())
            .collect();
        assert!(names.contains(&"ipxe-arm64.efi"));
    }

    #[test]
    fn tftp_manifest_marks_binary_flag() {
        let p = sample_params(BootMode::Uefi);
        let a = PxeConfigBuilder::build(&p);
        // 二进制项 is_binary=true
        let bin = a
            .tftp_manifest
            .iter()
            .find(|m| m.rel_path == "ipxe.efi")
            .unwrap();
        assert!(bin.is_binary);
        // 文本项 is_binary=false
        let txt = a
            .tftp_manifest
            .iter()
            .find(|m| m.rel_path == "bootstrap.ipxe")
            .unwrap();
        assert!(!txt.is_binary);
    }

    #[test]
    fn ipxe_script_idempotent() {
        // 同输入两次生成结果相同（纯函数）
        let p = sample_params(BootMode::Uefi);
        let s1 = PxeConfigBuilder::build_ipxe_script(&p);
        let s2 = PxeConfigBuilder::build_ipxe_script(&p);
        assert_eq!(s1, s2);
    }

    #[test]
    fn artifacts_find_file_missing() {
        let p = sample_params(BootMode::Uefi);
        let a = PxeConfigBuilder::build(&p);
        assert!(a.find_file("nonexistent").is_none());
    }

    // —— 覆盖率补测：模型 serde 往返 + boot_mode 全分支 ——

    #[test]
    fn boot_mode_default_ipxe_binary_all_variants() {
        // 覆盖 default_ipxe_binary 全分支
        assert_eq!(BootMode::Bios.default_ipxe_binary(), "undionly.kpxe");
        assert_eq!(BootMode::Uefi.default_ipxe_binary(), "ipxe.efi");
        assert_eq!(BootMode::UefiArm64.default_ipxe_binary(), "ipxe-arm64.efi");
    }

    #[test]
    fn boot_mode_serde_roundtrip() {
        // 覆盖 serde rename_all snake_case 往返
        let cases = [
            (BootMode::Bios, "bios"),
            (BootMode::Uefi, "uefi"),
            (BootMode::UefiArm64, "uefi_arm64"),
        ];
        for (mode, tag) in cases {
            let json = serde_json::to_string(&mode).unwrap();
            assert_eq!(json, format!("\"{tag}\""));
            let back: BootMode = serde_json::from_str(&json).unwrap();
            assert_eq!(back, mode);
        }
    }

    #[test]
    fn pxe_boot_params_serde_roundtrip() {
        let p = sample_params(BootMode::Uefi);
        let json = serde_json::to_string(&p).unwrap();
        let back: PxeBootParams = serde_json::from_str(&json).unwrap();
        assert_eq!(back.http_repo, p.http_repo);
        assert_eq!(back.kernel_path, p.kernel_path);
        assert_eq!(back.initramfs_path, p.initramfs_path);
        assert_eq!(back.base_image_path, p.base_image_path);
        assert_eq!(back.install_disk, p.install_disk);
        assert_eq!(back.tftp_server, p.tftp_server);
        assert_eq!(back.boot_mode, BootMode::Uefi);
    }

    #[test]
    fn dhcp_pxe_config_serde_roundtrip() {
        let c = DhcpPxeConfig {
            boot_filename: "ipxe.efi".into(),
            next_server: "10.0.0.1".into(),
        };
        let json = serde_json::to_string(&c).unwrap();
        let back: DhcpPxeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.boot_filename, "ipxe.efi");
        assert_eq!(back.next_server, "10.0.0.1");
    }

    #[test]
    fn tftp_manifest_entry_serde_roundtrip() {
        let e = TftpManifestEntry {
            rel_path: "undionly.kpxe".into(),
            source: "ipxe.org".into(),
            is_binary: true,
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: TftpManifestEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.rel_path, "undionly.kpxe");
        assert!(back.is_binary);
    }

    #[test]
    fn pxe_artifacts_find_file_returns_content() {
        // find_file 命中 → 返回 PxeFile 引用
        let p = sample_params(BootMode::Bios);
        let a = PxeConfigBuilder::build(&p);
        let f = a.find_file("bootstrap.ipxe").unwrap();
        assert!(f.content.starts_with("#!ipxe\n"));
        // pxelinux.cfg/default 也存在
        let cfg = a.find_file("pxelinux.cfg/default").unwrap();
        assert!(cfg.content.contains("DEFAULT ipxe"));
    }

    #[test]
    fn join_url_empty_path_returns_base() {
        // 覆盖 join_url 的 path 为空分支
        assert_eq!(join_url("http://a/b", ""), "http://a/b");
        assert_eq!(join_url("http://a/b/", ""), "http://a/b");
    }

    #[test]
    fn pxe_config_builder_default() {
        // 覆盖 PxeConfigBuilder::Default impl（unit struct，allow 抑制 clippy 简化提示，
        // 因为本测试目的就是调用 Default trait 方法）
        #![allow(clippy::default_constructed_unit_structs)]
        let _: PxeConfigBuilder = Default::default();
    }
}
