//! 裸机安装命令构造——纯函数，可单元测，不执行真实子进程。
//!
//! 设计动机（呼应 `cli.rs` 的 squashfs/xorriso 命令构造做法）：`RustInstaller::install`
//! 的核心可测逻辑是「为 7 步状态机的每一步构造正确的 sgdisk / zpool / unsquashfs / cp
//! / grub-install 命令参数」。把这些命令构造抽成纯函数（返回 `(program, Vec<String>)`
//! 或 `Vec<(program, Vec<String>)>`），就能在不写盘/不 spawn 子进程的前提下用断言
//! 验证 CLI 形态，避免依赖裸机 / root / gdisk / zfs 工具链。
//!
//! 命令类型：
//! - `(String, Vec<String>)`：单条命令——`(程序名, 程序名之后的参数列表)`。
//! - `Vec<(String, Vec<String>)>`：一个步骤产生多条命令（如多盘分区、装多个 binary）。
//!
//! 真实执行（spawn / 真写盘）留 `RustInstaller::install` 内 runner 注入（沙箱用
//! `FixtureIsoRunner`，生产用 `TokioIsoRunner`）。本模块**不**依赖 runner。
//!
//! 参考：规划文档 §3.11（安装器步骤）/ §3.19（首启重设密码）/ §10.2#17（HCL）。

// ----------------------------------------------------------------------------
// 步骤 1：分区（sgdisk 写 GPT 分区表）
// ----------------------------------------------------------------------------

/// 分区配置：单盘 EFI 512M + root 占满剩余空间（呼应 §3.11）。
///
/// 分区号约定：1 = EFI（512M，FAT32，UEFI 引导），2 = root（ZFS）。
/// 多盘 mirror/raidz 时每盘都重复同样的分区布局（root 分区组池）。
pub const EFI_PARTITION_INDEX: u32 = 1;
pub const ROOT_PARTITION_INDEX: u32 = 2;
/// EFI 分区大小（512 MiB，sgdisk 接受 `+512M` 写法）。
pub const EFI_PARTITION_SIZE: &str = "+512M";

/// 构造分区命令列表（每个目标盘 2 条 sgdisk：EFI + root）。
///
/// - 单盘（`raid` = None / `"stripe"`）：对 `/dev/sda` 执行
///   `sgdisk --new=1:0:+512M /dev/sda`（EFI）+ `sgdisk --new=2:0:0 /dev/sda`（root）
/// - mirror/raidz：对每块盘执行同样的两分区（root 分区 2 之后由 `create_pool_cmd` 组池）
///
/// 注：`--new=2:0:0` 的 `end_sector=0` 表示「占满剩余空间」。`raid` 参数当前不影响
/// 单盘分区布局（每盘都做 EFI+root 两分区），但保留以便未来按 RAID 调整（如 raidz
/// 可能需额外的小分区用于 ZFS 特殊 vdev）。
pub fn partition_cmd(disk: &str, _raid: Option<&str>) -> Vec<(String, Vec<String>)> {
    vec![
        (
            "sgdisk".to_string(),
            vec![
                format!("--new={EFI_PARTITION_INDEX}:0:{EFI_PARTITION_SIZE}"),
                disk.to_string(),
            ],
        ),
        (
            "sgdisk".to_string(),
            vec![
                format!("--new={ROOT_PARTITION_INDEX}:0:0"),
                disk.to_string(),
            ],
        ),
    ]
}

// ----------------------------------------------------------------------------
// 步骤 2：创建 ZFS 池（zpool create）
// ----------------------------------------------------------------------------

/// 构造 `zpool create` 命令。
///
/// 调用形态：
/// ```text
/// zpool create -f -o mountpoint=/ <pool> [<raid_level>] <part2_dev>...
/// ```
/// - `-f`：强制（无视既有分区表/池标签）
/// - `-o mountpoint=/`：根挂载到 `/`（altroot 由安装器在 chroot 前处理）
/// - RAID：`mirror` / `raidz1` / `raidz2` / `raidz3` 作为 zpool vdev 类型关键字，
///   出现在所有设备路径之前；`None` / `"stripe"` 不写关键字（zpool 默认 stripe）
///
/// 注：`disks` 应为目标盘列表；函数内部按 `<disk>2`（分区号 2 = root）派生 vdev 设备路径。
pub fn create_pool_cmd(
    disks: &[String],
    raid: Option<&str>,
    pool_name: &str,
) -> (String, Vec<String>) {
    let mut args = vec![
        "create".to_string(),
        "-f".to_string(),
        "-o".to_string(),
        "mountpoint=/".to_string(),
        pool_name.to_string(),
    ];
    // RAID vdev 关键字（mirror/raidzN）；stripe/None 无关键字
    if let Some(level) = raid {
        if !level.is_empty() && level != "stripe" {
            args.push(level.to_string());
        }
    }
    // 每盘的 root 分区（分区号 2）
    for d in disks {
        args.push(format!("{d}{ROOT_PARTITION_INDEX}"));
    }
    ("zpool".to_string(), args)
}

// ----------------------------------------------------------------------------
// 步骤 3：解压 rootfs（unsquashfs）
// ----------------------------------------------------------------------------

/// 构造 `unsquashfs -f -d <target> <squashfs>` 命令。
///
/// - `-f`：强制覆盖（重试安装时幂等）
/// - `-d <target>`：解压目标目录（通常是 `/target`，安装器先 mount 到此）
/// - `squashfs`：源 squashfs 镜像路径（ISO 内的 rootfs.squashfs）
pub fn extract_rootfs_cmd(squashfs: &str, target: &str) -> (String, Vec<String>) {
    (
        "unsquashfs".to_string(),
        vec![
            "-f".to_string(),
            "-d".to_string(),
            target.to_string(),
            squashfs.to_string(),
        ],
    )
}

// ----------------------------------------------------------------------------
// 步骤 4：安装组件二进制（cp）
// ----------------------------------------------------------------------------

/// 构造组件二进制安装命令列表（每个 binary 一条 cp）。
///
/// - `components`：binary 名列表（如 `["osd", "os-storage", "os-meta"]`）
/// - `target_bin`：目标盘的二进制目录（通常是 `/target/usr/local/bin`）
///
/// 每条命令：`cp /usr/local/bin/<name> <target_bin>/<name>`
///
/// 注：源路径假定 ISO 构建期已注入组件到 `/usr/local/bin`（呼应 `IsoSpec::components`）。
pub fn install_components_cmd(components: &[&str], target_bin: &str) -> Vec<(String, Vec<String>)> {
    components
        .iter()
        .map(|name| {
            (
                "cp".to_string(),
                vec![
                    format!("/usr/local/bin/{name}"),
                    format!("{target_bin}/{name}"),
                ],
            )
        })
        .collect()
}

// ----------------------------------------------------------------------------
// 步骤 5：配置系统（hostname / locale / admin 用户）
// ----------------------------------------------------------------------------

/// 构造系统配置命令列表（hostname + locale-gen + useradd）。
///
/// 产生的命令（顺序）：
/// 1. `sh -c 'echo <hostname> > /target/etc/hostname'`（写主机名）
/// 2. `locale-gen <locale>`（生成 locale，假设已 chroot 或 target 已挂载）
/// 3. `useradd -m -s /bin/bash -G sudo <admin>`（创建管理员，加入 sudo 组）
///
/// 注：`hostname` / `locale` / `admin` 均为非空校验过的合法值（由 `InstallTarget::validate`）。
/// 真实环境还可能需 `chroot /target` 包裹，此处返回扁平命令（runner 决定如何 spawn）。
pub fn configure_system_cmd(
    hostname: &str,
    locale: &str,
    admin: &str,
) -> Vec<(String, Vec<String>)> {
    vec![
        (
            "sh".to_string(),
            vec![
                "-c".to_string(),
                format!("echo {hostname} > /target/etc/hostname"),
            ],
        ),
        ("locale-gen".to_string(), vec![locale.to_string()]),
        (
            "useradd".to_string(),
            vec![
                "-m".to_string(),
                "-s".to_string(),
                "/bin/bash".to_string(),
                "-G".to_string(),
                "sudo".to_string(),
                admin.to_string(),
            ],
        ),
    ]
}

// ----------------------------------------------------------------------------
// 步骤 6：安装引导（grub-install，BIOS + UEFI 双装）
// ----------------------------------------------------------------------------

/// 构造 grub-install 命令列表（UEFI + BIOS 双装，呼应 §3.11 BIOS/UEFI 可启）。
///
/// 产生 2 条命令：
/// 1. `grub-install --target=x86_64-efi --boot-directory=<target>/boot --efi-directory=<target>/boot/efi <disk>`
///    （UEFI：写入 EFI 系统分区）
/// 2. `grub-install --target=i386-pc --boot-directory=<target>/boot <disk>`
///    （BIOS：写入磁盘 MBR/MBR gap）
///
/// - `disk`：目标盘（如 `/dev/sda`，整盘而非分区）
/// - `target`：解压后的根目录（如 `/target`，其下 `boot/` 与 `boot/efi/` 应已存在）
///
/// 注：UEFI 引导要求 EFI 系统分区已挂载到 `<target>/boot/efi`（分区步骤 + mount 由
/// 安装器编排，命令构造不验证）。
pub fn install_bootloader_cmd(disk: &str, target: &str) -> Vec<(String, Vec<String>)> {
    let boot_dir = format!("{target}/boot");
    let efi_dir = format!("{boot_dir}/efi");
    vec![
        (
            "grub-install".to_string(),
            vec![
                "--target=x86_64-efi".to_string(),
                format!("--boot-directory={boot_dir}"),
                format!("--efi-directory={efi_dir}"),
                disk.to_string(),
            ],
        ),
        (
            "grub-install".to_string(),
            vec![
                "--target=i386-pc".to_string(),
                format!("--boot-directory={boot_dir}"),
                disk.to_string(),
            ],
        ),
    ]
}

// ----------------------------------------------------------------------------
// 单元测试（纯函数，无工具链/裸机依赖）
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // —— partition_cmd ——

    #[test]
    fn partition_single_disk_two_commands() {
        let cmds = partition_cmd("/dev/sda", None);
        assert_eq!(cmds.len(), 2, "单盘应产 2 条（EFI + root）");
        // 第 1 条：EFI
        assert_eq!(cmds[0].0, "sgdisk");
        assert_eq!(
            cmds[0].1,
            vec!["--new=1:0:+512M".to_string(), "/dev/sda".to_string()]
        );
        // 第 2 条：root（占满）
        assert_eq!(cmds[1].0, "sgdisk");
        assert_eq!(
            cmds[1].1,
            vec!["--new=2:0:0".to_string(), "/dev/sda".to_string()]
        );
    }

    #[test]
    fn partition_efi_partition_index_is_1() {
        assert_eq!(EFI_PARTITION_INDEX, 1);
        assert_eq!(ROOT_PARTITION_INDEX, 2);
    }

    #[test]
    fn partition_efi_size_is_512m() {
        assert_eq!(EFI_PARTITION_SIZE, "+512M");
        let cmds = partition_cmd("/dev/sdb", None);
        assert!(cmds[0].1[0].contains("+512M"));
    }

    #[test]
    fn partition_stripe_same_as_none() {
        // stripe 与 None 在分区上等价（每盘 EFI+root）
        let none_cmds = partition_cmd("/dev/sda", None);
        let stripe_cmds = partition_cmd("/dev/sda", Some("stripe"));
        assert_eq!(none_cmds, stripe_cmds);
    }

    #[test]
    fn partition_mirror_same_layout_per_disk() {
        // mirror 当前不影响单盘分区布局（每盘都 EFI+root）
        let none_cmds = partition_cmd("/dev/sda", None);
        let mirror_cmds = partition_cmd("/dev/sda", Some("mirror"));
        assert_eq!(none_cmds, mirror_cmds);
    }

    #[test]
    fn partition_nvmename_path() {
        // NVMe 路径含 p（如 /dev/nvme0n1）——分区命令不关心命名
        let cmds = partition_cmd("/dev/nvme0n1", None);
        assert_eq!(cmds[1].1[1], "/dev/nvme0n1");
    }

    // —— create_pool_cmd ——

    #[test]
    fn create_pool_single_disk_stripe() {
        let (prog, args) = create_pool_cmd(&["/dev/sda".to_string()], None, "tank");
        assert_eq!(prog, "zpool");
        assert_eq!(args[0], "create");
        assert!(args.contains(&"-f".to_string()));
        assert!(args.contains(&"-o".to_string()));
        assert!(args.contains(&"mountpoint=/".to_string()));
        assert!(args.contains(&"tank".to_string()));
        // 无 vdev 关键字（stripe/None），直接接分区 2
        assert!(args.contains(&"/dev/sda2".to_string()));
        // 不应出现 mirror/raidz 关键字
        assert!(!args.contains(&"mirror".to_string()));
        assert!(!args.iter().any(|a| a.starts_with("raidz")));
    }

    #[test]
    fn create_pool_stripe_explicit() {
        let (_, args) = create_pool_cmd(&["/dev/sda".to_string()], Some("stripe"), "tank");
        // stripe 显式传也不应出现关键字
        assert!(!args.contains(&"stripe".to_string()));
        assert!(args.contains(&"/dev/sda2".to_string()));
    }

    #[test]
    fn create_pool_mirror_two_disks() {
        let (prog, args) = create_pool_cmd(
            &["/dev/sda".to_string(), "/dev/sdb".to_string()],
            Some("mirror"),
            "tank",
        );
        assert_eq!(prog, "zpool");
        // mirror 关键字应在 pool 名之后、设备之前
        let mirror_pos = args.iter().position(|a| a == "mirror").unwrap();
        let tank_pos = args.iter().position(|a| a == "tank").unwrap();
        assert!(mirror_pos > tank_pos, "mirror 应在 pool 名之后");
        // 两块盘的分区 2
        assert!(args.contains(&"/dev/sda2".to_string()));
        assert!(args.contains(&"/dev/sdb2".to_string()));
        let sda2_pos = args.iter().position(|a| a == "/dev/sda2").unwrap();
        assert!(sda2_pos > mirror_pos, "设备应在 mirror 之后");
    }

    #[test]
    fn create_pool_raidz1_three_disks() {
        let (_, args) = create_pool_cmd(
            &[
                "/dev/sda".to_string(),
                "/dev/sdb".to_string(),
                "/dev/sdc".to_string(),
            ],
            Some("raidz1"),
            "tank",
        );
        assert!(args.contains(&"raidz1".to_string()));
        assert!(args.contains(&"/dev/sda2".to_string()));
        assert!(args.contains(&"/dev/sdb2".to_string()));
        assert!(args.contains(&"/dev/sdc2".to_string()));
    }

    #[test]
    fn create_pool_raidz2_four_disks() {
        let disks: Vec<String> = ["/dev/sda", "/dev/sdb", "/dev/sdc", "/dev/sdd"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (_, args) = create_pool_cmd(&disks, Some("raidz2"), "tank");
        assert!(args.contains(&"raidz2".to_string()));
        for d in &disks {
            assert!(args.contains(&format!("{d}2")));
        }
    }

    #[test]
    fn create_pool_raidz3_five_disks() {
        let disks: Vec<String> = ["/dev/sda", "/dev/sdb", "/dev/sdc", "/dev/sdd", "/dev/sde"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (_, args) = create_pool_cmd(&disks, Some("raidz3"), "tank");
        assert!(args.contains(&"raidz3".to_string()));
        assert_eq!(disks.len(), 5);
    }

    #[test]
    fn create_pool_custom_pool_name() {
        let (_, args) = create_pool_cmd(&["/dev/sda".to_string()], None, "mypool");
        assert!(args.contains(&"mypool".to_string()));
        assert!(!args.contains(&"tank".to_string()));
    }

    #[test]
    fn create_pool_partition_suffix_is_2() {
        // vdev 设备路径必带分区号 2（root 分区）
        let (_, args) = create_pool_cmd(&["/dev/sda".to_string()], None, "tank");
        assert!(args.contains(&"/dev/sda2".to_string()));
        // 不应出现分区 1（EFI）作为 vdev
        assert!(!args.contains(&"/dev/sda1".to_string()));
    }

    #[test]
    fn create_pool_has_force_and_mountpoint_flags() {
        let (_, args) = create_pool_cmd(&["/dev/sda".to_string()], None, "tank");
        assert!(args.contains(&"-f".to_string()), "应强制 -f");
        assert!(args.contains(&"-o".to_string()));
        assert!(args.contains(&"mountpoint=/".to_string()));
    }

    // —— extract_rootfs_cmd ——

    #[test]
    fn extract_rootfs_shape() {
        let (prog, args) = extract_rootfs_cmd("rootfs.squashfs", "/target");
        assert_eq!(prog, "unsquashfs");
        assert_eq!(args[0], "-f", "应强制覆盖");
        assert_eq!(args[1], "-d");
        assert_eq!(args[2], "/target");
        assert_eq!(args[3], "rootfs.squashfs");
    }

    #[test]
    fn extract_rootfs_arg_count() {
        let (_, args) = extract_rootfs_cmd("a.squashfs", "/t");
        assert_eq!(args.len(), 4);
    }

    #[test]
    fn extract_rootfs_custom_paths() {
        let (prog, args) = extract_rootfs_cmd("/iso/casper/filesystem.squashfs", "/mnt/target");
        assert_eq!(prog, "unsquashfs");
        assert!(args.contains(&"/mnt/target".to_string()));
        assert!(args.contains(&"/iso/casper/filesystem.squashfs".to_string()));
    }

    // —— install_components_cmd ——

    #[test]
    fn install_components_one_per_binary() {
        let cmds =
            install_components_cmd(&["osd", "os-storage", "os-meta"], "/target/usr/local/bin");
        assert_eq!(cmds.len(), 3);
        assert_eq!(cmds[0].0, "cp");
        assert_eq!(
            cmds[0].1,
            vec![
                "/usr/local/bin/osd".to_string(),
                "/target/usr/local/bin/osd".to_string()
            ]
        );
        assert_eq!(cmds[1].1[1], "/target/usr/local/bin/os-storage");
        assert_eq!(cmds[2].1[1], "/target/usr/local/bin/os-meta");
    }

    #[test]
    fn install_components_empty() {
        let cmds = install_components_cmd(&[], "/target/usr/local/bin");
        assert!(cmds.is_empty());
    }

    #[test]
    fn install_components_single() {
        let cmds = install_components_cmd(&["osd"], "/t/bin");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].1[0], "/usr/local/bin/osd");
        assert_eq!(cmds[0].1[1], "/t/bin/osd");
    }

    #[test]
    fn install_components_source_path_is_usr_local_bin() {
        let cmds = install_components_cmd(&["x"], "/target/bin");
        // 源路径固定为 /usr/local/bin（ISO 构建期注入位置）
        assert_eq!(cmds[0].1[0], "/usr/local/bin/x");
    }

    // —— configure_system_cmd ——

    #[test]
    fn configure_system_three_commands() {
        let cmds = configure_system_cmd("os", "zh_CN.UTF-8", "admin");
        assert_eq!(cmds.len(), 3);
        // hostname
        assert_eq!(cmds[0].0, "sh");
        assert_eq!(cmds[0].1[0], "-c");
        assert!(cmds[0].1[1].contains("echo os > /target/etc/hostname"));
        // locale-gen
        assert_eq!(cmds[1].0, "locale-gen");
        assert_eq!(cmds[1].1, vec!["zh_CN.UTF-8".to_string()]);
        // useradd
        assert_eq!(cmds[2].0, "useradd");
        assert!(cmds[2].1.contains(&"-m".to_string()));
        assert!(cmds[2].1.contains(&"-s".to_string()));
        assert!(cmds[2].1.contains(&"/bin/bash".to_string()));
        assert!(cmds[2].1.contains(&"-G".to_string()));
        assert!(cmds[2].1.contains(&"sudo".to_string()));
        assert!(cmds[2].1.contains(&"admin".to_string()));
    }

    #[test]
    fn configure_system_hostname_dynamic() {
        let cmds = configure_system_cmd("mybox", "en_US.UTF-8", "ops");
        assert!(cmds[0].1[1].contains("mybox"));
        assert!(cmds[1].1[0] == "en_US.UTF-8");
        assert!(cmds[2].1.contains(&"ops".to_string()));
    }

    #[test]
    fn configure_system_useradd_includes_sudo_group() {
        let cmds = configure_system_cmd("h", "l", "u");
        let useradd = &cmds[2];
        assert_eq!(useradd.0, "useradd");
        // sudo 组成员资格确保管理员可提权
        let sudo_pos = useradd.1.iter().position(|a| a == "-G").unwrap();
        assert_eq!(useradd.1[sudo_pos + 1], "sudo");
    }

    // —— install_bootloader_cmd ——

    #[test]
    fn bootloader_two_commands_uefi_and_bios() {
        let cmds = install_bootloader_cmd("/dev/sda", "/target");
        assert_eq!(cmds.len(), 2, "应装 UEFI + BIOS 两份");
        // UEFI
        assert_eq!(cmds[0].0, "grub-install");
        assert!(cmds[0].1.contains(&"--target=x86_64-efi".to_string()));
        assert!(cmds[0]
            .1
            .contains(&"--boot-directory=/target/boot".to_string()));
        assert!(cmds[0]
            .1
            .contains(&"--efi-directory=/target/boot/efi".to_string()));
        assert!(cmds[0].1.contains(&"/dev/sda".to_string()));
        // BIOS
        assert_eq!(cmds[1].0, "grub-install");
        assert!(cmds[1].1.contains(&"--target=i386-pc".to_string()));
        assert!(cmds[1]
            .1
            .contains(&"--boot-directory=/target/boot".to_string()));
        assert!(cmds[1].1.contains(&"/dev/sda".to_string()));
    }

    #[test]
    fn bootloader_uefi_has_efi_directory() {
        let cmds = install_bootloader_cmd("/dev/sda", "/target");
        // 仅 UEFI 命令应有 --efi-directory
        assert!(cmds[0].1.iter().any(|a| a.starts_with("--efi-directory=")));
        assert!(!cmds[1].1.iter().any(|a| a.starts_with("--efi-directory=")));
    }

    #[test]
    fn bootloader_bios_target_is_i386_pc() {
        let cmds = install_bootloader_cmd("/dev/sda", "/target");
        assert!(cmds[1].1.contains(&"--target=i386-pc".to_string()));
    }

    #[test]
    fn bootloader_disk_is_whole_disk_not_partition() {
        let cmds = install_bootloader_cmd("/dev/sda", "/target");
        // grub-install 接受整盘（MBR / ESP），不应是分区号
        assert!(cmds[0].1.contains(&"/dev/sda".to_string()));
        assert!(!cmds[0].1.iter().any(|a| a.contains("/dev/sda1")));
        assert!(!cmds[0].1.iter().any(|a| a.contains("/dev/sda2")));
    }

    #[test]
    fn bootloader_boot_directory_uses_target() {
        let cmds = install_bootloader_cmd("/dev/nvme0n1", "/mnt");
        assert!(cmds[0]
            .1
            .contains(&"--boot-directory=/mnt/boot".to_string()));
        assert!(cmds[1]
            .1
            .contains(&"--boot-directory=/mnt/boot".to_string()));
    }

    // —— 综合：RAID 级别差异 ——

    #[test]
    fn raid_level_differs_in_pool_cmd() {
        let disks_two: Vec<String> = ["/dev/sda", "/dev/sdb"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let disks_three: Vec<String> = ["/dev/sda", "/dev/sdb", "/dev/sdc"]
            .iter()
            .map(|s| s.to_string())
            .collect();

        let mirror = create_pool_cmd(&disks_two, Some("mirror"), "tank");
        let raidz1 = create_pool_cmd(&disks_three, Some("raidz1"), "tank");

        assert!(mirror.1.contains(&"mirror".to_string()));
        assert!(!mirror.1.contains(&"raidz1".to_string()));
        assert!(raidz1.1.contains(&"raidz1".to_string()));
        assert!(!raidz1.1.contains(&"mirror".to_string()));
    }

    #[test]
    fn multi_disk_vs_single_disk_partition() {
        // 多盘时每盘单独 partition_cmd（调用方循环）；此处验证单盘函数可复用
        let single = partition_cmd("/dev/sda", None);
        let disk_b = partition_cmd("/dev/sdb", Some("mirror"));
        // 两者结构相同（仅盘名不同）
        assert_eq!(single.len(), disk_b.len());
        assert_eq!(single[0].1[0], disk_b[0].1[0], "EFI 分区参数应一致");
        assert_eq!(single[0].1[1], "/dev/sda");
        assert_eq!(disk_b[0].1[1], "/dev/sdb");
    }

    #[test]
    fn full_pipeline_all_six_steps_produce_commands() {
        // 端到端：6 步全跑一遍，验证每步都产命令（综合烟测）
        // 1. 分区
        let part = partition_cmd("/dev/sda", None);
        assert!(!part.is_empty());
        // 2. 建池
        let pool = create_pool_cmd(&["/dev/sda".to_string()], None, "tank");
        assert!(!pool.1.is_empty());
        // 3. 解 rootfs
        let extract = extract_rootfs_cmd("rootfs.squashfs", "/target");
        assert!(!extract.1.is_empty());
        // 4. 装组件
        let comps = install_components_cmd(&["osd"], "/target/usr/local/bin");
        assert!(!comps.is_empty());
        // 5. 配置
        let conf = configure_system_cmd("os", "zh_CN.UTF-8", "admin");
        assert!(!conf.is_empty());
        // 6. 引导
        let boot = install_bootloader_cmd("/dev/sda", "/target");
        assert!(!boot.is_empty());
    }
}
