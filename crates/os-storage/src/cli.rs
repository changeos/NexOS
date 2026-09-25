//! ZFS CLI 命令构造——纯函数，可单元测，不执行真实子进程。
//!
//! 设计动机：`ZfsCliBackend` 的核心可测逻辑是「构造正确的 zpool/zfs 命令参数」。
//! 把命令构造抽成纯函数（返回 `Vec<String>` 参数列表），就能在不 spawn 子进程的前提下
//! 用断言验证 CLI 形态，避免依赖真实 ZFS 环境（开发机通常无 ZFS，规格书 §6 要求沙箱）。
//!
//! 命名：`zpool_*` / `zfs_*` 对应 CLI 工具名；返回 `Vec<String>` 是「程序名之后的参数」
//! （调用方在此基础上 `Command::new("zpool").args(...)`）。
//!
//! 输出格式统一用 `-p -H`（机器可读、tab 分隔、精确整数），解析逻辑在 [`crate::model`]。

use crate::model::VdevSpec;
use crate::options::{Atime, Compression, DatasetOptions};

// ----------------------------------------------------------------------------
// zpool 命令
// ----------------------------------------------------------------------------

/// 构造 `zpool create <pool> [vdev-spec...]` 的参数列表。
///
/// vdev 规格展开：`mirror d1 d2` / `raidz1 d1 d2 d3` / 单盘 `d1`。
/// 多个 vdev 顺序拼接（如 mirror + mirror = 双镜像组）。
///
/// 注意：实际执行需 root（`zpool create` 写磁盘标签并加载内核模块）。
pub(crate) fn zpool_create_args(pool: &str, vdevs: &[VdevSpec]) -> Vec<String> {
    // -f 强制创建：清除磁盘上已有分区表（BitLocker/GPT/MBR 等）
    // 用户 2026-08-23 实测：新 NVMe 有 Windows BitLocker 分区导致创建失败
    let mut args = vec!["create".to_string(), "-f".to_string(), pool.to_string()];
    for v in vdevs {
        let kw = v.kind.as_zpool_keyword();
        if !kw.is_empty() {
            args.push(kw.to_string());
        }
        args.extend(v.disks.iter().cloned());
    }
    args
}

/// 构造 `zpool destroy <pool>` 的参数列表（高危！）。
pub(crate) fn zpool_destroy_args(pool: &str) -> Vec<String> {
    vec!["destroy".to_string(), pool.to_string()]
}

/// 构造 `zpool list -p -H` 的参数列表（列所有池；可附 `-o` 选字段）。
pub(crate) fn zpool_list_args() -> Vec<String> {
    vec!["list".to_string(), "-p".to_string(), "-H".to_string()]
}

/// 构造 `zpool status [pool]` 的参数列表。
///
/// 与 `zpool list` 不同，`zpool status` **不加** `-p -H`——它的 vdev 明细只有
/// 默认的人类可读树形格式（缩进表深度，NAME/STATE/READ/WRITE/CKSUM 列）。
/// 解析逻辑见 [`crate::backend_impl::parse_zpool_status`]。
///
/// `pool` 为 None 时列所有池；Some 时仅列该池（含其 vdev 树）。
pub(crate) fn zpool_status_args(pool: Option<&str>) -> Vec<String> {
    let mut args = vec!["status".to_string()];
    if let Some(p) = pool {
        args.push(p.to_string());
    }
    args
}

// ----------------------------------------------------------------------------
// zfs 命令
// ----------------------------------------------------------------------------

/// 构造 `zfs create [-o prop=val ...] [-V volsize] <dataset>` 的参数列表。
///
/// `DatasetOptions` 各字段映射到 `-o`：
/// - compression → `-o compression=lz4|gzip|zstd|off`
/// - atime → `-o atime=on|off|relatime`
/// - recordsize → `-o recordsize=<n>`
/// - quota.refquota → `-o refquota=<n>`
/// - quota.refreservation → `-o refreservation=<n>`
/// - reservation → `-o reservation=<n>`
/// - dedup → `-o dedup=on|off`
/// - mountpoint → `-o mountpoint=<path>`
/// - volsize → 用 `-V <size>`（创建 zvol；此时是块设备而非文件系统）
///
/// 加密相关 `encryption`/`keylocation`/`keyformat` 也映射到 `-o`，但密钥材料
/// （passphrase）由 [`crate::crypto`] 在调用时通过 stdin 注入，不进参数列表（敏感，不落命令行）。
pub(crate) fn zfs_create_args(dataset: &str, opts: &DatasetOptions) -> Vec<String> {
    let mut args = vec!["create".to_string()];

    if let Some(volsize) = opts.volsize {
        // zvol：-V 指定容量，创建块设备
        args.push("-V".to_string());
        args.push(volsize.to_string());
    }

    if let Some(c) = &opts.compression {
        args.push("-o".to_string());
        args.push(format!("compression={}", compression_value(c)));
    }
    if let Some(a) = opts.atime {
        args.push("-o".to_string());
        args.push(format!("atime={}", atime_value(a)));
    }
    if let Some(rs) = opts.recordsize {
        args.push("-o".to_string());
        args.push(format!("recordsize={rs}"));
    }
    if let Some(q) = opts.quota {
        if let Some(rq) = q.refquota {
            args.push("-o".to_string());
            args.push(format!("refquota={rq}"));
        }
        if let Some(rr) = q.refreservation {
            args.push("-o".to_string());
            args.push(format!("refreservation={rr}"));
        }
    }
    if let Some(r) = opts.reservation {
        args.push("-o".to_string());
        args.push(format!("reservation={r}"));
    }
    if let Some(d) = opts.dedup {
        args.push("-o".to_string());
        args.push(format!("dedup={}", if d { "on" } else { "off" }));
    }
    if let Some(mp) = &opts.mountpoint {
        args.push("-o".to_string());
        args.push(format!("mountpoint={mp}"));
    }
    if let Some(enc) = &opts.encryption {
        if enc.enabled {
            if let Some(cipher) = &enc.cipher {
                args.push("-o".to_string());
                args.push(format!("encryption={cipher}"));
            }
            if let Some(kl) = &enc.keylocation {
                args.push("-o".to_string());
                args.push(format!("keylocation={kl}"));
            }
            if let Some(kf) = &enc.keyformat {
                args.push("-o".to_string());
                args.push(format!("keyformat={kf}"));
            }
        }
    }

    args.push(dataset.to_string());
    args
}

/// 构造 `zfs destroy <dataset>` 的参数列表（`-r` 递归销毁子项与快照，高危！）。
pub(crate) fn zfs_destroy_args(dataset: &str) -> Vec<String> {
    vec!["destroy".to_string(), "-r".to_string(), dataset.to_string()]
}

/// 构造 `zfs destroy <dataset>@<snap>` 的参数列表（销毁快照）。
pub(crate) fn zfs_destroy_snapshot_args(snapshot: &str) -> Vec<String> {
    vec!["destroy".to_string(), snapshot.to_string()]
}

/// 构造 `zfs list -p -H -o name,used,avail,mounted,encryption [pool]` 参数列表。
/// `pool` 为 None 时列全池；Some 时仅列该池（含子数据集）。
pub(crate) fn zfs_list_datasets_args(pool: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "list".to_string(),
        "-p".to_string(),
        "-H".to_string(),
        "-o".to_string(),
        "name,used,avail,mounted,encryption".to_string(),
        "-t".to_string(),
        "filesystem,volume".to_string(),
    ];
    if let Some(p) = pool {
        args.push("-r".to_string());
        args.push(p.to_string());
    }
    args
}

/// 构造 `zfs snapshot <dataset>@<name>` 的参数列表。
pub(crate) fn zfs_snapshot_args(dataset: &str, name: &str) -> Vec<String> {
    vec!["snapshot".to_string(), format!("{dataset}@{name}")]
}

/// 构造 `zfs list -t snapshot -p -H -o name,used,creation [dataset]` 参数列表。
/// `dataset` 为 None 时列全池快照；Some 时仅列该数据集快照。
pub(crate) fn zfs_list_snapshots_args(dataset: Option<&str>) -> Vec<String> {
    let mut args = vec![
        "list".to_string(),
        "-t".to_string(),
        "snapshot".to_string(),
        "-p".to_string(),
        "-H".to_string(),
        "-o".to_string(),
        "name,used,creation".to_string(),
    ];
    if let Some(d) = dataset {
        args.push("-r".to_string());
        args.push(d.to_string());
    }
    args
}

/// 构造 `zfs set refquota=<n>|refreservation=<n> <dataset>` 参数列表。
pub(crate) fn zfs_set_quota_args(
    dataset: &str,
    refquota: Option<u64>,
    refreservation: Option<u64>,
) -> Vec<String> {
    let mut args = vec!["set".to_string()];
    let mut props = Vec::new();
    if let Some(rq) = refquota {
        props.push(format!("refquota={rq}"));
    }
    if let Some(rr) = refreservation {
        props.push(format!("refreservation={rr}"));
    }
    // 至少设置一个属性；调用方保证 quota 非空
    let prop_str = if props.is_empty() {
        "refquota=0".to_string()
    } else {
        props.join(",")
    };
    args.push(prop_str);
    args.push(dataset.to_string());
    args
}

/// 构造 `zfs get -p -H -o value refquota,refreservation <dataset>` 参数列表。
pub(crate) fn zfs_get_quota_args(dataset: &str) -> Vec<String> {
    vec![
        "get".to_string(),
        "-p".to_string(),
        "-H".to_string(),
        "-o".to_string(),
        "value".to_string(),
        "refquota,refreservation".to_string(),
        dataset.to_string(),
    ]
}

// ----------------------------------------------------------------------------
// 辅助：属性值格式化
// ----------------------------------------------------------------------------

/// 压缩算法 → zfs 属性值字符串。
fn compression_value(c: &Compression) -> String {
    match c {
        Compression::Off => "off".to_string(),
        Compression::Lz4 => "lz4".to_string(),
        Compression::Gzip => "gzip".to_string(),
        Compression::Zstd => "zstd".to_string(),
        Compression::Custom(s) => s.clone(),
    }
}

/// atime → zfs 属性值字符串。
fn atime_value(a: Atime) -> &'static str {
    match a {
        Atime::On => "on",
        Atime::Off => "off",
        Atime::Relatime => "relatime",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EncryptionConfig, Quota, VdevKind};

    #[test]
    fn zpool_create_single_disk() {
        let vdevs = vec![VdevSpec {
            kind: VdevKind::Disk,
            disks: vec!["/dev/sdb".into()],
        }];
        let args = zpool_create_args("tank", &vdevs);
        assert_eq!(args, vec!["create", "-f", "tank", "/dev/sdb"]);
    }

    #[test]
    fn zpool_create_mirror_then_raidz1() {
        let vdevs = vec![
            VdevSpec {
                kind: VdevKind::Mirror,
                disks: vec!["/dev/sdb".into(), "/dev/sdc".into()],
            },
            VdevSpec {
                kind: VdevKind::Raidz1,
                disks: vec!["/dev/sdd".into(), "/dev/sde".into(), "/dev/sdf".into()],
            },
        ];
        let args = zpool_create_args("tank", &vdevs);
        assert_eq!(
            args,
            vec![
                "create", "-f", "tank", "mirror", "/dev/sdb", "/dev/sdc", "raidz1", "/dev/sdd",
                "/dev/sde", "/dev/sdf"
            ]
        );
    }

    #[test]
    fn zpool_destroy_and_list() {
        assert_eq!(zpool_destroy_args("old"), vec!["destroy", "old"]);
        assert_eq!(zpool_list_args(), vec!["list", "-p", "-H"]);
    }

    #[test]
    fn zpool_status_args_all_and_scoped() {
        // 全池：仅 `status`（不带 -p -H——vdev 树形明细只有人类可读格式）。
        assert_eq!(zpool_status_args(None), vec!["status"]);
        // 单池：追加池名。
        let scoped = zpool_status_args(Some("tank"));
        assert_eq!(scoped, vec!["status", "tank"]);
    }

    #[test]
    fn zfs_create_filesystem_minimal() {
        let opts = DatasetOptions::default();
        let args = zfs_create_args("tank/media", &opts);
        assert_eq!(args, vec!["create", "tank/media"]);
    }

    #[test]
    fn zfs_create_zvol_with_options() {
        let opts = DatasetOptions {
            volsize: Some(1_073_741_824),
            compression: Some(Compression::Lz4),
            atime: Some(Atime::Off),
            recordsize: Some(1_048_576),
            quota: Some(Quota {
                refquota: Some(500_000_000),
                refreservation: None,
            }),
            reservation: None,
            dedup: Some(true),
            mountpoint: None,
            encryption: None,
        };
        let args = zfs_create_args("tank/vol0", &opts);
        // volsize 走 -V，其余 -o
        assert!(args.starts_with(&[
            "create".to_string(),
            "-V".to_string(),
            "1073741824".to_string()
        ]));
        assert!(args.contains(&"-o".to_string()));
        assert!(args.iter().any(|a| a == "compression=lz4"));
        assert!(args.iter().any(|a| a == "atime=off"));
        assert!(args.iter().any(|a| a == "recordsize=1048576"));
        assert!(args.iter().any(|a| a == "refquota=500000000"));
        assert!(args.iter().any(|a| a == "dedup=on"));
        // 末尾是 dataset 名
        assert_eq!(args.last(), Some(&"tank/vol0".to_string()));
    }

    #[test]
    fn zfs_create_encrypted_no_passphrase_in_args() {
        let opts = DatasetOptions {
            encryption: Some(EncryptionConfig {
                enabled: true,
                cipher: Some("aes-256-gcm".into()),
                keylocation: Some("prompt".into()),
                keyformat: Some("passphrase".into()),
            }),
            ..Default::default()
        };
        let args = zfs_create_args("vault/secret", &opts);
        assert!(args.iter().any(|a| a == "encryption=aes-256-gcm"));
        assert!(args.iter().any(|a| a == "keylocation=prompt"));
        assert!(args.iter().any(|a| a == "keyformat=passphrase"));
        // 确保无 passphrase 明文（passphrase 经 stdin，不应进参数）
        assert!(!args
            .iter()
            .any(|a| a.contains("passphrase=") && a != "keyformat=passphrase"));
    }

    #[test]
    fn zfs_list_datasets_args_all_and_scoped() {
        assert_eq!(
            zfs_list_datasets_args(None),
            vec![
                "list",
                "-p",
                "-H",
                "-o",
                "name,used,avail,mounted,encryption",
                "-t",
                "filesystem,volume"
            ]
        );
        let scoped = zfs_list_datasets_args(Some("tank"));
        assert_eq!(scoped.last(), Some(&"tank".to_string()));
        assert!(scoped.contains(&"-r".to_string()));
    }

    #[test]
    fn zfs_snapshot_and_list_args() {
        assert_eq!(
            zfs_snapshot_args("tank/media", "snap1"),
            vec!["snapshot", "tank/media@snap1"]
        );
        assert_eq!(
            zfs_list_snapshots_args(None),
            vec![
                "list",
                "-t",
                "snapshot",
                "-p",
                "-H",
                "-o",
                "name,used,creation"
            ]
        );
        let scoped = zfs_list_snapshots_args(Some("tank/media"));
        assert_eq!(scoped.last(), Some(&"tank/media".to_string()));
    }

    #[test]
    fn zfs_quota_args() {
        assert_eq!(
            zfs_set_quota_args("tank/media", Some(1000), Some(500)),
            vec!["set", "refquota=1000,refreservation=500", "tank/media"]
        );
        // 单属性
        assert_eq!(
            zfs_set_quota_args("tank/media", None, Some(500)),
            vec!["set", "refreservation=500", "tank/media"]
        );
        // 空时退化（调用方应避免，但不应 panic）
        assert_eq!(
            zfs_set_quota_args("tank/media", None, None),
            vec!["set", "refquota=0", "tank/media"]
        );
        assert_eq!(
            zfs_get_quota_args("tank/media"),
            vec![
                "get",
                "-p",
                "-H",
                "-o",
                "value",
                "refquota,refreservation",
                "tank/media"
            ]
        );
    }
}
