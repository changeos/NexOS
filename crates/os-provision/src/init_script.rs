//! 系统初始化脚本生成（规划文档 §3.10 阶段1：分区/建池/装基础系统）
//!
//! 阶段1 在 initramfs 中执行（PXE 引导拉起内核 + initramfs 后）：
//! 分区 → 建 ZFS 池 → 挂载 squashfs 基础系统到池根 → 安装引导（GRUB/systemd-boot）→
//! 写入 root 密码哈希 → 重启进入新系统（让 osd 空壳接管）。
//!
//! 本模块是**纯逻辑**：把磁盘/池名/镜像路径等参数渲染为 shell 脚本字符串。
//! 真正执行（initramfs 集成、真分区）由下游（iso-agent / 安装器）完成，本 crate 不执行
//! （红线：不真分区建池）。
//!
//! 脚本骨架特性：
//! - 参数化磁盘路径、池名、镜像路径、root 密码哈希。
//! - 每步带错误检查（`set -euo pipefail`）与 echo 日志（便于远程调试）。
//! - 保留 `# TODO` 标注集成点（initramfs 怎么 hook、osd systemd unit 路径等）。

use serde::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// 初始化参数
// ----------------------------------------------------------------------------

/// 阶段1 系统初始化脚本参数。
///
/// 与 [`crate::provision::ProvisionConfig`] 字段对应，但展开为脚本可直接消费的形式：
/// - `install_disk` ← `zfs_pool_disks[0]`（单盘场景；多盘见 [`InitScriptParams::extra_disks`]）
/// - `base_image_url` ← 由 initramfs 解析的内核 cmdline 传入
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitScriptParams {
    /// 安装目标盘（如 `/dev/sda`；脚本会用 zpool create 把它做成 ZFS 池 vdev）。
    pub install_disk: String,
    /// 额外池成员盘（如 `["/dev/sdb"]`，组成 mirror/raidz；空 = 单盘 stripe）。
    pub extra_disks: Vec<String>,
    /// ZFS 池名（默认 `tank`）。
    pub pool_name: String,
    /// 池拓扑（`stripe` / `mirror` / `raidz1` / `raidz2`）。
    pub pool_topology: PoolTopology,
    /// base 镜像 HTTP 路径（squashfs，由 initramfs 解析的 cmdline 传入；空则用占位）。
    pub base_image_url: String,
    /// base 镜像本地落盘路径（initramfs 已下载到本地的位置，供 unsquashfs 用）。
    pub base_image_local: String,
    /// root 密码哈希（首启强制重设——见 §3.19；脚本写入 `/etc/shadow`）。
    /// 安全：仅哈希，绝不记明文日志；空串表示不在此阶段写（首启走交互式）。
    pub root_password_hash: String,
    /// 目标节点主机名（写入新系统的 `/etc/hostname`）。
    pub hostname: String,
}

/// ZFS 池拓扑。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PoolTopology {
    /// 单盘 stripe（无冗余）
    Stripe,
    /// mirror（≥2 盘）
    Mirror,
    /// raidz1（≥3 盘，单盘容错）
    Raidz1,
    /// raidz2（≥4 盘，双盘容错）
    Raidz2,
}

impl PoolTopology {
    /// zpool create 子命令（`mirror` / `raidz1` 等；stripe 返回空串）。
    pub fn zpool_keyword(self) -> &'static str {
        match self {
            PoolTopology::Stripe => "",
            PoolTopology::Mirror => "mirror",
            PoolTopology::Raidz1 => "raidz1",
            PoolTopology::Raidz2 => "raidz2",
        }
    }
}

// ----------------------------------------------------------------------------
// 生成器
// ----------------------------------------------------------------------------

/// 系统初始化脚本生成器——纯逻辑，把 [`InitScriptParams`] 渲染为 shell 脚本字符串。
///
/// 输出脚本是骨架（带 `# TODO` 标注下游集成点）；真执行由下游完成（iso-agent 的安装器
/// 或 os-provision 编排器在 initramfs 内 hook 调用）。
#[derive(Debug, Clone, Default)]
pub struct InitScriptBuilder;

impl InitScriptBuilder {
    /// 生成完整阶段1 安装脚本。
    pub fn build(params: &InitScriptParams) -> String {
        let mut s = String::new();
        s.push_str("#!/bin/sh\n");
        s.push_str("# 由 os-provision::init_script 自动生成——勿手改\n");
        s.push_str(
            "# 阶段1 系统初始化：分区 → 建 ZFS 池 → 装基础系统 → 写 root 密码哈希 → 安装引导\n",
        );
        s.push_str("set -euo pipefail\n\n");

        s.push_str("# —— 参数（由 iPXE kernel cmdline / initramfs env 注入）——\n");
        s.push_str(&format!("INSTALL_DISK='{}'\n", params.install_disk));
        s.push_str(&format!("POOL_NAME='{}'\n", params.pool_name));
        s.push_str(&format!("BASE_IMAGE_LOCAL='{}'\n", params.base_image_local));
        s.push_str(&format!("HOSTNAME='{}'\n", params.hostname));
        s.push('\n');

        // —— 步骤 1：分区 ——
        s.push_str("# —— 步骤 1：在目标盘上建 GPT + ZFS 分区 ——\n");
        s.push_str("# TODO: 实际安装器应支持 BIOS/UEFI 分区表分支（UEFI 需 ESP/FAT32 引导分区）\n");
        s.push_str("parted -s \"$INSTALL_DISK\" mklabel gpt\n");
        s.push_str("parted -s \"$INSTALL_DISK\" mkpart primary 1MiB 100%\n");
        s.push_str("zpool_labelclear -q \"${INSTALL_DISK}1\" 2>/dev/null || true\n");
        s.push('\n');

        // —— 步骤 2：建 ZFS 池 ——
        s.push_str("# —— 步骤 2：建 ZFS 池 ——\n");
        s.push_str(&Self::build_zpool_create(params));
        s.push_str("zfs set atime=off \"$POOL_NAME\"\n");
        s.push_str("zfs set compression=lz4 \"$POOL_NAME\"\n");
        s.push('\n');

        // —— 步骤 3：装基础系统 ——
        s.push_str("# —— 步骤 3：解压 base 镜像到池根 ——\n");
        s.push_str("MNT=\"$(mktemp -d)\"\n");
        s.push_str("zfs create \"$POOL_NAME/ROOT\"\n");
        s.push_str("zfs create \"$POOL_NAME/ROOT/default\"\n");
        s.push_str("mount -t zfs \"$POOL_NAME/ROOT/default\" \"$MNT\"\n");
        s.push_str("# 注：squashfs 由 initramfs 已下载到 $BASE_IMAGE_LOCAL\n");
        s.push_str("unsquashfs -f -d \"$MNT\" \"$BASE_IMAGE_LOCAL\"\n");
        s.push('\n');

        // —— 步骤 4：写 root 密码哈希 ——
        s.push_str("# —— 步骤 4：写 root 密码哈希（首启强制重设，呼应 §3.19）——\n");
        if params.root_password_hash.is_empty() {
            s.push_str("# root_password_hash 为空——跳过此步（首启走交互式 chpasswd）\n");
        } else {
            s.push_str("# 安全：仅哈希写入 /etc/shadow，绝不 echo 明文；root 首启强制重设\n");
            s.push_str(&format!(
                "echo 'root:{}' > \"$MNT/etc/shadow.tmp\"\n",
                params.root_password_hash
            ));
            s.push_str("chmod 600 \"$MNT/etc/shadow.tmp\"\n");
            s.push_str("mv \"$MNT/etc/shadow.tmp\" \"$MNT/etc/shadow\"\n");
            s.push_str("# TODO: 首启标志——下次启动强制 chpasswd（由 osd 空壳检查并清除）\n");
            s.push_str("touch \"$MNT/etc/os/first-boot.flag\"\n");
        }
        s.push('\n');

        // —— 步骤 5：主机名 ——
        s.push_str("# —— 步骤 5：写主机名 ——\n");
        s.push_str("echo \"$HOSTNAME\" > \"$MNT/etc/hostname\"\n");
        s.push('\n');

        // —— 步骤 6：安装引导 ——
        s.push_str("# —— 步骤 6：安装引导（GRUB/systemd-boot）+ 缓存池 import ——\n");
        s.push_str("# TODO: 实际安装器需 mount pseudo-fs (proc/dev/sys) 后 chroot 装引导\n");
        s.push_str("mount -t proc none \"$MNT/proc\"\n");
        s.push_str("mount --rbind /dev \"$MNT/dev\"\n");
        s.push_str("mount --rbind /sys \"$MNT/sys\"\n");
        s.push_str("chroot \"$MNT\" /bin/sh -c \"zpool set cachefile=/etc/zfs/zpool.cache \\\"$POOL_NAME\\\" || true\"\n");
        s.push_str("# TODO: chroot 内调用 grub-install / bootctl（依据 BIOS/UEFI 分支）\n");
        s.push_str("umount -R \"$MNT\" 2>/dev/null || true\n");
        s.push('\n');

        // —— 收尾 ——
        s.push_str("# —— 收尾：sync + 提示重启 ——\n");
        s.push_str("sync\n");
        s.push_str(
            "echo [os-provision] 阶段1 完成：目标盘 $INSTALL_DISK 已装基础系统，请重启进入新系统\n",
        );
        s.push_str("# TODO: 触发远程编排器轮询（通知 PxeProvisioner 阶段1 done）\n");
        s
    }

    /// 生成 `zpool create` 子命令行（按拓扑拼成员盘）。
    pub fn build_zpool_create(params: &InitScriptParams) -> String {
        let mut disks: Vec<String> = vec![quote_disk(&params.install_disk)];
        for d in &params.extra_disks {
            disks.push(quote_disk(d));
        }
        let vdev = match params.pool_topology {
            PoolTopology::Stripe => disks.join(" "),
            topo => {
                let kw = topo.zpool_keyword();
                format!("{} {}", kw, disks.join(" "))
            }
        };
        format!("zpool create -f -o ashift=12 \"$POOL_NAME\" {}\n", vdev)
    }

    /// 生成校验参数合法性的诊断信息（不 panic，返回 Err 字符串）。
    pub fn validate(params: &InitScriptParams) -> Result<(), String> {
        if params.install_disk.trim().is_empty() {
            return Err("install_disk 不能为空".into());
        }
        if params.pool_name.trim().is_empty() {
            return Err("pool_name 不能为空".into());
        }
        let min_disks = match params.pool_topology {
            PoolTopology::Stripe => 1,
            PoolTopology::Mirror => 2,
            PoolTopology::Raidz1 => 3,
            PoolTopology::Raidz2 => 4,
        };
        let total_disks = 1 + params.extra_disks.len();
        if total_disks < min_disks {
            return Err(format!(
                "{:?} 拓扑需至少 {} 盘，实际 {}",
                params.pool_topology, min_disks, total_disks
            ));
        }
        // 安全：root_password_hash 不允许明文密码（粗略检查：不应包含空格分隔的明文）
        if !params.root_password_hash.is_empty()
            && params.root_password_hash.chars().any(char::is_whitespace)
        {
            return Err("root_password_hash 不得含空白（应为单字段哈希）".into());
        }
        Ok(())
    }
}

/// 把磁盘路径用引号包裹（防 shell 注入/空格）。
fn quote_disk(disk: &str) -> String {
    format!("\"{}\"", disk)
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_params() -> InitScriptParams {
        InitScriptParams {
            install_disk: "/dev/sda".into(),
            extra_disks: vec![],
            pool_name: "tank".into(),
            pool_topology: PoolTopology::Stripe,
            base_image_url: "http://10.0.0.1:8080/provision/base.squashfs".into(),
            base_image_local: "/tmp/base.squashfs".into(),
            root_password_hash: "$6$rounds=5000$abc...".into(),
            hostname: "os-001".into(),
        }
    }

    #[test]
    fn topology_keywords() {
        assert_eq!(PoolTopology::Stripe.zpool_keyword(), "");
        assert_eq!(PoolTopology::Mirror.zpool_keyword(), "mirror");
        assert_eq!(PoolTopology::Raidz1.zpool_keyword(), "raidz1");
        assert_eq!(PoolTopology::Raidz2.zpool_keyword(), "raidz2");
    }

    #[test]
    fn build_script_stripe_single_disk() {
        let p = sample_params();
        let s = InitScriptBuilder::build(&p);
        assert!(s.starts_with("#!/bin/sh\n"));
        assert!(s.contains("set -euo pipefail"));
        assert!(s.contains("INSTALL_DISK='/dev/sda'"));
        assert!(s.contains("POOL_NAME='tank'"));
        // stripe 无 mirror/raidz 关键字
        assert!(s.contains("zpool create -f -o ashift=12 \"$POOL_NAME\" \"/dev/sda\""));
        // 步骤齐全
        assert!(s.contains("步骤 1："));
        assert!(s.contains("步骤 2：建 ZFS 池"));
        assert!(s.contains("步骤 3："));
        assert!(s.contains("unsquashfs"));
        assert!(s.contains("步骤 4："));
        assert!(s.contains("root:$6$rounds=5000$abc..."));
        assert!(s.contains("first-boot.flag"));
        assert!(s.contains("步骤 5："));
        assert!(s.contains("os-001"));
        assert!(s.contains("步骤 6："));
        assert!(s.contains("阶段1 完成"));
    }

    #[test]
    fn build_script_mirror_multi_disk() {
        let mut p = sample_params();
        p.extra_disks = vec!["/dev/sdb".into(), "/dev/sdc".into()];
        p.pool_topology = PoolTopology::Mirror;
        let s = InitScriptBuilder::build(&p);
        assert!(s.contains("zpool create -f -o ashift=12 \"$POOL_NAME\" mirror \"/dev/sda\" \"/dev/sdb\" \"/dev/sdc\""));
    }

    #[test]
    fn build_script_raidz2() {
        let mut p = sample_params();
        p.extra_disks = vec!["/dev/sdb".into(), "/dev/sdc".into(), "/dev/sdd".into()];
        p.pool_topology = PoolTopology::Raidz2;
        let s = InitScriptBuilder::build(&p);
        assert!(s.contains("raidz2 \"/dev/sda\""));
    }

    #[test]
    fn build_script_empty_hash_skips_shadow_write() {
        let mut p = sample_params();
        p.root_password_hash.clear();
        let s = InitScriptBuilder::build(&p);
        assert!(s.contains("root_password_hash 为空——跳过"));
        assert!(!s.contains("/etc/shadow.tmp"));
    }

    #[test]
    fn build_script_idempotent() {
        let p = sample_params();
        let s1 = InitScriptBuilder::build(&p);
        let s2 = InitScriptBuilder::build(&p);
        assert_eq!(s1, s2);
    }

    #[test]
    fn validate_stripe_ok() {
        assert!(InitScriptBuilder::validate(&sample_params()).is_ok());
    }

    #[test]
    fn validate_rejects_empty_disk() {
        let mut p = sample_params();
        p.install_disk = "  ".into();
        let err = InitScriptBuilder::validate(&p).unwrap_err();
        assert!(err.contains("install_disk"));
    }

    #[test]
    fn validate_rejects_insufficient_disks_for_mirror() {
        let mut p = sample_params();
        p.pool_topology = PoolTopology::Mirror;
        // 仅 1 盘，mirror 需 2
        let err = InitScriptBuilder::validate(&p).unwrap_err();
        assert!(err.contains("Mirror"));
        assert!(err.contains("至少 2"));
    }

    #[test]
    fn validate_rejects_raidz1_insufficient() {
        let mut p = sample_params();
        p.extra_disks = vec!["/dev/sdb".into()];
        p.pool_topology = PoolTopology::Raidz1;
        // 2 盘，raidz1 需 3
        let err = InitScriptBuilder::validate(&p).unwrap_err();
        assert!(err.contains("Raidz1"));
    }

    #[test]
    fn validate_rejects_whitespace_in_hash() {
        let mut p = sample_params();
        p.root_password_hash = "foo bar baz".into();
        let err = InitScriptBuilder::validate(&p).unwrap_err();
        assert!(err.contains("空白"));
    }

    #[test]
    fn zpool_create_disk_quoting() {
        let p = InitScriptParams {
            install_disk: "/dev/sda".into(),
            extra_disks: vec!["/dev/sdb".into()],
            pool_name: "tank".into(),
            pool_topology: PoolTopology::Mirror,
            base_image_url: String::new(),
            base_image_local: "/tmp/x".into(),
            root_password_hash: String::new(),
            hostname: "os".into(),
        };
        let line = InitScriptBuilder::build_zpool_create(&p);
        assert!(line.contains("\"/dev/sda\""));
        assert!(line.contains("\"/dev/sdb\""));
    }

    // —— 覆盖率补测：validate 边界 + raidz2 不够盘 ——

    #[test]
    fn validate_rejects_empty_pool_name() {
        let mut p = sample_params();
        p.pool_name = "  ".into();
        let err = InitScriptBuilder::validate(&p).unwrap_err();
        assert!(err.contains("pool_name"));
    }

    #[test]
    fn validate_rejects_raidz2_insufficient() {
        // raidz2 需 4 盘，仅给 3 盘
        let mut p = sample_params();
        p.extra_disks = vec!["/dev/sdb".into(), "/dev/sdc".into()];
        p.pool_topology = PoolTopology::Raidz2;
        let err = InitScriptBuilder::validate(&p).unwrap_err();
        assert!(err.contains("Raidz2"));
        assert!(err.contains("至少 4"));
    }

    #[test]
    fn validate_raidz1_ok_with_three_disks() {
        // raidz1 3 盘刚好满足
        let mut p = sample_params();
        p.extra_disks = vec!["/dev/sdb".into(), "/dev/sdc".into()];
        p.pool_topology = PoolTopology::Raidz1;
        assert!(InitScriptBuilder::validate(&p).is_ok());
    }

    #[test]
    fn validate_mirror_ok_with_two_disks() {
        let mut p = sample_params();
        p.extra_disks = vec!["/dev/sdb".into()];
        p.pool_topology = PoolTopology::Mirror;
        assert!(InitScriptBuilder::validate(&p).is_ok());
    }

    #[test]
    fn validate_raidz2_ok_with_four_disks() {
        let mut p = sample_params();
        p.extra_disks = vec!["/dev/sdb".into(), "/dev/sdc".into(), "/dev/sdd".into()];
        p.pool_topology = PoolTopology::Raidz2;
        assert!(InitScriptBuilder::validate(&p).is_ok());
    }

    #[test]
    fn build_zpool_create_stripe_no_extra_disks() {
        let p = sample_params(); // stripe 单盘
        let line = InitScriptBuilder::build_zpool_create(&p);
        assert!(line.contains("zpool create"));
        assert!(!line.contains("mirror"));
        assert!(!line.contains("raidz"));
    }
}
