//! SMB/NFS 真实落盘 + reload 接通集成测（batch8）。
//!
//! 区别于 batch5（`smb_real.rs`，只验证渲染语法 + testparm）与 batch6（`nfs_real.rs`，
//! 只验证渲染语法 + exportfs option 关键字），本测聚焦 **batch5/6 留下的 TODO [RUNTIME]**
//! ——把 `SambaOrchestrator::write_smb_conf` / `reload_smbd` / `NfsOrchestrator::apply_exports`
//! 从「只渲染不落盘」推进到「真实落盘 + 真实工具往返」。
//!
//! ## 接通点（本批新接通的 [RUNTIME]）
//! - `SambaOrchestrator::write_smb_conf`：真实 `tokio::fs::write` 到 `config.config_path`
//!   （默认 `/etc/samba/smb.conf`，本测注入 `/tmp/...`）；
//! - `SambaOrchestrator::reload_smbd`：`smbcontrol all reload-config`（经 [`ReloadPolicy`]，
//!   本测用 `DryRun`/`Disabled` 不碰宿主 smbd）；
//! - `NfsOrchestrator::apply_exports`：渲染 exports 落盘到 `exports_path` +
//!   `exportfs -i -o <opts> <client>:<path>` 逐条落入内核 export 表（`Enabled`）；
//!   `remove_export` 经 `exportfs -u <client>:<path>` 幂等撤销。
//!
//! ## 测试矩阵（4 个，全部 `#[ignore]`——需本机 samba/nfs + 部分 root）
//! - **a. write_smb_conf 真实落盘往返**：render + write 到 /tmp → testparm 校验落盘文件 →
//!   读回验证内容 → RAII 清理（非特权）。
//! - **b. exportfs 临时 export 往返**：add_export → apply_exports 落盘 + `exportfs -i` 落
//!   内核表 → `exportfs -v` 验证临时 export 存在 → remove_export → `exportfs -u` 撤销 →
//!   RAII 清理（需 root）。
//! - **c. smbcontrol 可达性（不真 reload 宿主）**：用 `ReloadPolicy::DryRun` 验证命令构造 +
//!   `smbcontrol -s <tmp> all reload-config --configfile=<tmp>`（smbcontrol 支持 `-s`，对临时
//!   配置构造 reload 命令；本机无运行 smbd 时会失败，本测只验证命令可达 + DryRun 不 spawn）。
//! - **d. 完整 SMB 编排往返**：create_share → write_smb_conf 落盘 → testparm 验证 →
//!   delete_share → 重写 → 验证更新（RAII 清理，非特权）。
//!
//! ## 红线（规格 §9 / 任务说明）
//! - **绝不**碰 `/etc/samba/smb.conf` / `/etc/exports` / 宿主 smbd / nfsd；
//! - 全部经 `with_reload(config, tmp_path, ReloadPolicy::*)` 注入临时配置 + 策略；
//! - 落内核 export 表的 b 测用 `/tmp` 临时路径 + RAII `exportfs -u` 撤销（幂等）；
//! - reload 宿主 smbd 的 c 测只走 `DryRun` + `smbcontrol -s <tmp>`（不找运行中 smbd）。
//!
//! ## 跑法
//! ```bash
//! cargo build -p os-protocols --features mock
//! # a/d（非特权，需 testparm）：
//! cargo test -p os-protocols --features mock --test smb_nfs_integrate_real -- --ignored --nocapture
//! # b（需 root + exportfs）：
//! sudo cargo test -p os-protocols --features mock --test smb_nfs_integrate_real -- --ignored --nocapture
//! ```

#![cfg(feature = "mock")]

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use os_protocols::common::{FileProtocol, Protocol, Share};
use os_protocols::nfs::{NfsExportOptions, NfsManager};
use os_protocols::smb::SmbManager;
use os_protocols::{
    GaneshaConfig, NfsOrchestrator, ReloadPolicy, SambaConfig, SambaOrchestrator, ShareId,
    ShareOptions,
};

use chrono::Utc;

// ============================================================================
// 辅助：纯 Rust 的 `which` / `is_root`（与 smb_real.rs / nfs_real.rs 一致）
// ============================================================================

/// 扫 `$PATH` 找可执行文件。
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

/// 是否以 root 运行（exportfs 落内核 export 表要求 root）。
fn is_root() -> bool {
    Command::new("id").arg("-u").output().ok().and_then(|o| {
        String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse::<u32>()
            .ok()
    }) == Some(0)
}

/// 构造一个测试用 Share（SMB 或 NFS，按 protocol 参数）。
fn share(id: &str, name: &str, protocol: Protocol, path: &str) -> Share {
    Share {
        id: ShareId::new(id),
        name: name.into(),
        protocol,
        path: PathBuf::from(path),
        read_only: false,
        hosts_allow: vec![],
        enabled: true,
        created_at: Utc::now(),
    }
}

/// 跑 `testparm -s <file>` 校验语法，返回 (exit_ok, combined_output)。
fn run_testparm(file: &PathBuf) -> (bool, String) {
    let out = Command::new("testparm")
        .arg("-s")
        .arg(file)
        .output()
        .expect("spawn testparm 失败（samba 未装？）");
    let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
    combined.push_str(&String::from_utf8_lossy(&out.stderr));
    (out.status.success(), combined)
}

// ============================================================================
// a. write_smb_conf 真实落盘往返（非特权；需 testparm）
// ============================================================================

/// `write_smb_conf` 真实落盘：render → write 到 /tmp 临时 smb.conf → testparm 校验落盘文件
/// 语法 → 读回验证内容含 [global] + [share] 段。RAII（tempdir）自动清理。
///
/// 验证点：
/// - `write_smb_conf` 真的把渲染产物写入 `config.config_path`（非骨架"只返回路径"）；
/// - 落盘文件经真实 testparm 子进程校验，语法被 samba 4.23 接受（exit 0 + Loaded OK）；
/// - 读回内容与 `render_conf()` 一致（含共享段、path、valid users）。
///
/// 红线：注入 `/tmp` 临时 config_path + `Disabled` reload（不跑 smbcontrol），不碰 /etc/samba。
#[tokio::test]
#[ignore = "需本机 testparm（非特权）。跑法：cargo test -p os-protocols --features mock --test smb_nfs_integrate_real -- --ignored --nocapture"]
async fn real_write_smb_conf_lands_to_tmp_and_passes_testparm() {
    if which("testparm").is_none() {
        eprintln!("[integrate] SKIP real_write_smb_conf: testparm 不在 $PATH");
        return;
    }

    // 注入临时 config_path + Disabled reload（不碰 /etc/samba + 不跑 smbcontrol）
    let tmp = tempfile::tempdir_in("/tmp").expect("建临时目录失败");
    let conf_path = tmp.path().join("smb.conf");
    let mut cfg = SambaConfig::defaults();
    cfg.config_path = conf_path.clone();
    let orch = SambaOrchestrator::with_reload(cfg, ReloadPolicy::Disabled);

    // 注册一个共享（走编排器真实 async 生命周期）
    let s = share("s1", "media", Protocol::Smb, "/tank/media");
    orch.create_share(
        s.clone(),
        ShareOptions {
            comment: Some("媒体库".into()),
            valid_users: vec!["alice".into(), "bob".into()],
            ..ShareOptions::default()
        },
    )
    .await
    .unwrap();

    // 落盘前的渲染产物（用于与落盘文件比对）
    let rendered = orch.render_conf();
    assert!(rendered.contains("[media]"));
    assert!(rendered.contains("valid users = alice bob"));

    // 真实落盘（write_smb_conf 接通点）
    let path = orch.write_smb_conf().await.expect("write_smb_conf 失败");
    assert_eq!(path, conf_path, "write_smb_conf 应返回注入的临时路径");

    // 落盘文件确实存在 + 内容与渲染产物一致
    let written = fs::read_to_string(&conf_path).expect("读回落盘 smb.conf 失败");
    assert_eq!(written, rendered, "落盘内容应与 render_conf() 完全一致");

    // 真实 testparm 校验落盘文件语法
    let (ok, output) = run_testparm(&conf_path);
    assert!(
        ok,
        "落盘 smb.conf testparm 校验失败：\n--- 落盘文件 ---\n{written}\n--- testparm ---\n{output}"
    );
    assert!(
        output.contains("Loaded services file OK."),
        "testparm 未报告 OK：{output}"
    );
    // testparm 展开后保留共享段关键信息（被 samba 接受）
    assert!(output.contains("/tank/media"), "testparm 产物缺 media path");
    assert!(
        output.contains("alice") && output.contains("bob"),
        "testparm 产物缺 valid users"
    );
    eprintln!("[integrate] write_smb_conf 落盘 + testparm 校验通过。落盘路径：{conf_path:?}");
    // tmp tempdir Drop 时自动清理
}

// ============================================================================
// b. exportfs 临时 export 往返（需 root + exportfs）
// ============================================================================

/// `apply_exports` 真实 exportfs 往返：add_export → apply_exports 落盘 + `exportfs -i` 落内核
/// 表 → `exportfs -v` 验证临时 export 存在 → remove_export → `exportfs -u` 撤销 → RAII 清理。
///
/// 验证点：
/// - `apply_exports`（Enabled）真的把每条 export 经 `exportfs -i -o <opts> <client>:<path>`
///   落入内核 export 表（非骨架"只渲染"）；
/// - `exportfs -v` 能看到刚落的临时 export（路径 + option 串）；
/// - `remove_export` 经 `exportfs -u` 幂等撤销，`exportfs -v` 不再见该 export。
///
/// 红线：用 `/tmp` 下唯一临时目录作为 export 路径 + `exportfs -i`（忽略 /etc/exports）+
/// RAII（tempdir + Drop 时 exportfs -u 兜底）；**绝不**碰 /etc/exports、不改既有 export。
#[tokio::test]
#[ignore = "需 root + exportfs（会临时往内核 export 表加 /tmp 路径，RAII 撤销；不碰 /etc/exports）。跑法：sudo cargo test -p os-protocols --features mock --test smb_nfs_integrate_real -- --ignored --nocapture"]
async fn real_apply_exports_round_trips_via_exportfs() {
    if which("exportfs").is_none() {
        eprintln!("[integrate] SKIP real_apply_exports: exportfs 不在 $PATH");
        return;
    }
    if !is_root() {
        eprintln!(
            "[integrate] SKIP real_apply_exports: 非 root（exportfs 落内核 export 要求 root，\
             跑法：sudo cargo test ... -- --ignored）"
        );
        return;
    }

    // 用 /tmp 下唯一临时目录作为被 export 的真实路径（exportfs 要求路径存在）
    let export_root = tempfile::tempdir_in("/tmp").expect("建临时 export 根目录失败");
    let export_dir = export_root.path().join("nfs-real-share");
    fs::create_dir_all(&export_dir).expect("建临时 export 目录失败");
    let export_path_str = export_dir.to_string_lossy().to_string();

    // 注入临时 exports_path + Enabled reload（真跑 exportfs）
    let exports_tmp = tempfile::tempdir_in("/tmp").expect("建临时 exports 文件目录失败");
    let exports_path = exports_tmp.path().join("exports");
    let orch = NfsOrchestrator::with_reload(
        GaneshaConfig::defaults(),
        exports_path.clone(),
        ReloadPolicy::Enabled,
    );

    // 注册共享 + export（client 用 127.0.0.1，option 默认 rw,sync,root_squash）
    let s = share("n1", "media", Protocol::Nfs, &export_path_str);
    orch.create_share(s.clone(), ShareOptions::default())
        .await
        .unwrap();
    orch.add_export(
        &ShareId::new("n1"),
        vec!["127.0.0.1".into()],
        NfsExportOptions::default(),
    )
    .await
    .expect("add_export（含 apply_exports + exportfs -i）失败");

    // apply_exports 应已落盘到临时 exports 文件
    let written = fs::read_to_string(&exports_path).expect("读回 exports 失败");
    eprintln!("[integrate] 落盘 exports 文件：\n{written}");
    assert!(
        written.contains(&format!("{export_path_str} 127.0.0.1(rw,sync,root_squash)")),
        "落盘 exports 缺预期行：{written}"
    );

    // exportfs -v 验证内核 export 表含刚落的临时 export
    let out_v = Command::new("exportfs")
        .arg("-v")
        .output()
        .expect("spawn exportfs -v 失败");
    let stdout_v = String::from_utf8_lossy(&out_v.stdout);
    eprintln!("[integrate] exportfs -v（应用后）：\n{stdout_v}");
    assert!(
        stdout_v.contains(&export_path_str),
        "exportfs -v 未见刚落的临时 export {export_path_str}：{stdout_v}"
    );
    // option 串应含 rw（默认 read_write=true）
    assert!(
        stdout_v.contains("rw"),
        "exportfs -v 未见 rw option：{stdout_v}"
    );

    // remove_export → unexport_share + 重写 exports 文件
    orch.remove_export(&ShareId::new("n1"))
        .await
        .expect("remove_export 失败");

    // exportfs -v 验证临时 export 已撤销（不再含该路径）
    let out_v2 = Command::new("exportfs")
        .arg("-v")
        .output()
        .expect("spawn exportfs -v 失败");
    let stdout_v2 = String::from_utf8_lossy(&out_v2.stdout);
    eprintln!("[integrate] exportfs -v（撤销后）：\n{stdout_v2}");
    assert!(
        !stdout_v2.contains(&export_path_str),
        "exportfs -u 后仍见临时 export（撤销失败）：{stdout_v2}"
    );

    // 兜底：再 exportfs -u 一次（幂等；防 Drop 前的残留）
    let _ = Command::new("exportfs")
        .args(["-u", &format!("127.0.0.1:{export_path_str}")])
        .output();
    eprintln!("[integrate] apply_exports exportfs 往返通过。");
    // export_root / exports_tmp Drop 时自动清理
}

// ============================================================================
// c. smbcontrol 可达性（不真 reload 宿主 smbd）
// ============================================================================

/// `reload_smbd` 可达性 + 命令构造验证：用 `ReloadPolicy::DryRun` 验证编排器构造的命令正确
/// 且不 spawn；再用 `smbcontrol -s <tmp> all reload-config` 探测 smbcontrol 二进制可达
/// （`-s` 指定临时配置文件，`all` 目标在本机无运行 smbd 时会失败——本测把这种"无 smbd"
/// 视作预期，只断言 smbcontrol 二进制能被 spawn + 能解析 `-s`/`all`/`reload-config` 参数）。
///
/// 验证点：
/// - `ReloadPolicy::DryRun` 下 `reload_smbd` 立即成功（不 spawn，不碰宿主）；
/// - `smbcontrol` 二进制可达（spawn 不 panic）；
/// - smbcontrol 接受 `-s <tmp> all reload-config` 参数形态（本机无 smbd 时退出码非 0 是
///   预期的"No daemons active"，本测把这种情况与 exit 0 一并视作"命令构造正确"——只断言
///   spawn 成功 + stderr/输出可读，不断言退出码，避免绑定"本机必须运行 smbd"）。
///
/// 红线：**绝不**真 reload 宿主 smbd——DryRun 不 spawn；`smbcontrol -s <tmp>` 虽 spawn 但
/// `-s` 指向临时配置，且 `all` 在无 smbd 时无害退出（不找指定 PID 的 smbd）。
#[tokio::test]
#[ignore = "需本机 smbcontrol（非特权，不碰宿主 smbd）。跑法：cargo test -p os-protocols --features mock --test smb_nfs_integrate_real -- --ignored --nocapture"]
async fn real_reload_smbd_dry_run_and_smbcontrol_reachable() {
    if which("smbcontrol").is_none() {
        eprintln!("[integrate] SKIP real_reload_smbd: smbcontrol 不在 $PATH");
        return;
    }

    // 1. DryRun：reload_smbd 立即成功（不 spawn 子进程）
    let tmp = tempfile::tempdir_in("/tmp").expect("建临时目录失败");
    let conf_path = tmp.path().join("smb.conf");
    let mut cfg = SambaConfig::defaults();
    cfg.config_path = conf_path.clone();
    let orch = SambaOrchestrator::with_reload(cfg, ReloadPolicy::DryRun);
    // DryRun reload 不应出错（只打印命令）
    orch.reload_smbd()
        .await
        .expect("DryRun reload_smbd 应成功（不 spawn）");
    assert_eq!(orch.reload_policy(), ReloadPolicy::DryRun);

    // 2. Disabled：完全跳过（也是立即成功）
    let orch2 = SambaOrchestrator::with_reload(SambaConfig::defaults(), ReloadPolicy::Disabled);
    orch2
        .reload_smbd()
        .await
        .expect("Disabled reload_smbd 应成功（跳过）");

    // 3. smbcontrol 二进制可达 + 接受 `-s <tmp> all reload-config` 参数形态。
    //    先落一个临时 smb.conf（经 write_smb_conf），用 `-s` 喂给 smbcontrol。
    let orch3 = SambaOrchestrator::with_reload(
        {
            let mut c = SambaConfig::defaults();
            c.config_path = conf_path.clone();
            c
        },
        ReloadPolicy::Disabled,
    );
    orch3
        .write_smb_conf()
        .await
        .expect("落盘临时 smb.conf 失败");

    let out = Command::new("smbcontrol")
        .arg("-s")
        .arg(&conf_path)
        .args(["all", "reload-config"])
        .output()
        .expect("spawn smbcontrol 失败");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    eprintln!(
        "[integrate] smbcontrol -s <tmp> all reload-config exit={:?}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        out.status.code()
    );
    // 本机无运行 smbd 时，smbcontrol 退出码非 0 + stderr 含 "No daemons active" 或类似——
    // 这是预期的（我们不要求本机运行 smbd）。只断言 spawn 成功 + 输出可读。
    // 但若 stderr 含 "invalid option"/"unrecognized" 则是参数构造错（需修）。
    assert!(
        !stderr.to_lowercase().contains("unrecognized option")
            && !stderr.to_lowercase().contains("invalid option"),
        "smbcontrol 拒绝参数形态（构造错）：{stderr}"
    );
    eprintln!("[integrate] reload_smbd DryRun/Disabled + smbcontrol 可达性验证通过。");
}

// ============================================================================
// d. 完整 SMB 编排往返（非特权；需 testparm）
// ============================================================================

/// 完整 SMB 编排往返：create_share → write_smb_conf 落盘 → testparm 验证 →
/// delete_share → 重写 → 验证更新（共享段消失）。RAII 清理。
///
/// 验证点：
/// - create_share 后 write_smb_conf 落盘的文件含 [share] 段；
/// - delete_share 后重写，落盘文件不再含 [share] 段（编排器状态与落盘一致）；
/// - 每次落盘后 testparm 校验通过（语法始终被 samba 接受）。
///
/// 红线：注入 `/tmp` 临时 config_path + Disabled reload，不碰 /etc/samba。
#[tokio::test]
#[ignore = "需本机 testparm（非特权）。跑法：cargo test -p os-protocols --features mock --test smb_nfs_integrate_real -- --ignored --nocapture"]
async fn real_smb_orchestration_full_round_trip() {
    if which("testparm").is_none() {
        eprintln!("[integrate] SKIP real_smb_full_round_trip: testparm 不在 $PATH");
        return;
    }

    let tmp = tempfile::tempdir_in("/tmp").expect("建临时目录失败");
    let conf_path = tmp.path().join("smb.conf");
    let mut cfg = SambaConfig::defaults();
    cfg.config_path = conf_path.clone();
    let orch = SambaOrchestrator::with_reload(cfg, ReloadPolicy::Disabled);

    // 1. create_share + write_smb_conf → 落盘含 [media]
    let s = share("s1", "media", Protocol::Smb, "/tank/media");
    orch.create_share(
        s.clone(),
        ShareOptions {
            comment: Some("媒体库".into()),
            ..ShareOptions::default()
        },
    )
    .await
    .unwrap();
    orch.write_smb_conf()
        .await
        .expect("首次 write_smb_conf 失败");
    let written1 = fs::read_to_string(&conf_path).expect("读回落盘 smb.conf 失败");
    assert!(written1.contains("[global]"), "落盘文件缺 [global] 段");
    assert!(written1.contains("[media]"), "落盘文件缺 [media] 段");
    assert!(written1.contains("path = /tank/media"));
    // testparm 校验
    let (ok1, out1) = run_testparm(&conf_path);
    assert!(ok1, "首次落盘 testparm 失败：{out1}");
    eprintln!("[integrate] 首次落盘 testparm 通过。");

    // 2. delete_share + 重写 → 落盘不再含 [media]
    orch.delete_share(&ShareId::new("s1")).await.unwrap();
    orch.write_smb_conf()
        .await
        .expect("删除后 write_smb_conf 失败");
    let written2 = fs::read_to_string(&conf_path).expect("读回落盘 smb.conf 失败");
    assert!(
        written2.contains("[global]"),
        "删除后落盘文件仍应含 [global]"
    );
    assert!(
        !written2.contains("[media]"),
        "删除后落盘文件不应再含 [media]"
    );
    assert!(
        !written2.contains("/tank/media"),
        "删除后落盘文件不应再含 media path"
    );
    // testparm 校验（只剩 [global]，仍应合法）
    let (ok2, out2) = run_testparm(&conf_path);
    assert!(ok2, "删除后落盘 testparm 失败：{out2}");
    eprintln!("[integrate] 删除后落盘 testparm 通过。");

    // 3. 再 create_share 不同名 → 落盘含新 [docs]
    let s2 = share("s2", "docs", Protocol::Smb, "/tank/docs");
    orch.create_share(s2, ShareOptions::default())
        .await
        .unwrap();
    orch.write_smb_conf()
        .await
        .expect("二次 write_smb_conf 失败");
    let written3 = fs::read_to_string(&conf_path).expect("读回落盘 smb.conf 失败");
    assert!(written3.contains("[docs]"), "二次落盘文件缺 [docs] 段");
    assert!(written3.contains("path = /tank/docs"));
    assert!(!written3.contains("[media]"), "二次落盘不应含旧 [media]");
    let (ok3, out3) = run_testparm(&conf_path);
    assert!(ok3, "二次落盘 testparm 失败：{out3}");
    eprintln!(
        "[integrate] 完整 SMB 编排往返通过（create → write → delete → rewrite → recreate）。"
    );
}
