//! 网络装机（P0）真实测：真实 live-server ISO 的 casper 引导件提取。
//!
//! 与 pxe_real.rs 同款分层：
//! - **A. 引导链渲染测（默认跑，纯逻辑）**：bootstrap.ipxe → 动态菜单 → boot
//!   条目 → seed 四件拼接成完整链路快照断言（os-provision 纯函数，端到端串起来）。
//! - **B. 真实 ISO 提取测（`#[ignore]`，需本机 ISO 仓有文件）**：对仓内每件
//!   `ubuntu-*-live-server-*.iso` 跑 [`iso9660::extract_file`] 提取
//!   `casper/vmlinuz` + `casper/initrd`，断言非空/体积量级/幻数（x86 bzImage 的
//!   `MZ` EFI stub、arm64 Image 的 `ARMd`、initrd 的 gzip/zstd 头）。
//!
//! ISO 仓目录：env `NEXOS_PROVISION_REPO`（缺省 `/tank/os-data/provision`）下
//! `isos/`。仓为空时优雅 SKIP（不 panic）。
//!
//! 运行：
//! ```bash
//! cargo test -p os-provision --test netboot_real                # A 默认跑
//! cargo test -p os-provision --test netboot_real -- --ignored --nocapture  # B 真实 ISO
//! ```

use std::fs;
use std::io::Read;
use std::path::PathBuf;

use os_provision::iso9660;
use os_provision::netboot::{
    self, bootstrap_ipxe_script, ipxe_menu_script, mark_latest_stable, parse_iso_repo_filename,
    render_base, render_user_data, AutoinstallSeedParams, CASPER_INITRD, CASPER_VMLINUZ,
};

// ============================================================================
// A. 引导链渲染（默认跑，纯逻辑）
// ============================================================================

/// A1：完整引导链四件（bootstrap → 菜单 → boot 条目 → 种子）按同一来源地址渲染，
/// 相互引用的 URL 严丝合缝（menu 的 chain 入口 ↔ bootstrap 的下一跳；boot 条目的
/// kernel/initrd/url/seed ↔ 流式直传与种子端点）。
#[test]
fn a1_boot_chain_urls_are_consistent() {
    let mut entries = vec![
        parse_iso_repo_filename("ubuntu-26.04.1-live-server-amd64.iso").unwrap(),
        parse_iso_repo_filename("ubuntu-26.04.1-live-server-arm64.iso").unwrap(),
    ];
    mark_latest_stable(&mut entries);

    let base = "http://192.0.2.106:8558";
    let bootstrap = bootstrap_ipxe_script();
    let menu = render_base(&ipxe_menu_script(&entries, "amd64", "efi"), base);

    // bootstrap 下一跳 = 菜单端点
    assert!(bootstrap.contains("/api/v1/provisioning/ipxe/menu"));
    // 菜单：列最新稳定版 + 引导件 URL 指向流式直传路由
    assert!(menu.contains("Ubuntu Server 26.04.1（amd64）"));
    assert!(menu.contains(&format!(
        "{base}/api/v1/provisioning/boot/26.04.1-amd64/vmlinuz"
    )));
    assert!(menu.contains(&format!(
        "{base}/api/v1/provisioning/boot/26.04.1-amd64/initrd"
    )));
    assert!(menu.contains(&format!(
        "{base}/api/v1/provisioning/isos/ubuntu-26.04.1-live-server-amd64.iso"
    )));
    assert!(menu.contains(&format!("{base}/api/v1/provisioning/seed/26.04.1-amd64/")));

    // 种子与菜单同源：late-commands 拉同一 base 的 install.sh
    let mut p = AutoinstallSeedParams::default();
    p.version = "26.04.1".into();
    p.arch = "amd64".into();
    p.server_base = base.into();
    let ud = render_user_data(&p).unwrap();
    assert!(ud.contains(&format!("curl -fsSL {base}/api/v1/provisioning/install.sh")));
}

/// A2：seed 生成对非法 id 形态（穿越/杂字符）整体拒绝——菜单/种子只服务白名单条目。
#[test]
fn a2_seed_rejects_non_repo_shapes() {
    for (v, a) in [
        ("../26.04.1", "amd64"),
        ("26.04.1", "../amd64"),
        ("26;04", "amd64"),
    ] {
        let mut p = AutoinstallSeedParams::default();
        p.version = v.into();
        p.arch = a.into();
        assert!(render_user_data(&p).is_err(), "应拒绝 version={v} arch={a}");
    }
}

// ============================================================================
// B. 真实 ISO casper 提取（#[ignore]，需 ISO 仓有文件）
// ============================================================================

/// ISO 仓 isos 目录（env 覆盖）。
fn repo_isos_dir() -> PathBuf {
    std::env::var_os("NEXOS_PROVISION_REPO")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tank/os-data/provision"))
        .join("isos")
}

/// 列出仓内全部可解析的 live-server ISO（解析失败的非仓件跳过）。
fn repo_isos() -> Vec<(PathBuf, os_provision::netboot::IsoRepoEntry)> {
    let dir = repo_isos_dir();
    let Ok(rd) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if let Some(e) = parse_iso_repo_filename(&name) {
            out.push((ent.path(), e));
        }
    }
    out
}

/// B1：真实 ISO 提取 casper/vmlinuz + casper/initrd（每件仓内 ISO 一轮）。
#[test]
#[ignore = "真实 ISO：需本机 ISO 仓（/tank/os-data/provision/isos），人工 --ignored 跑"]
fn b1_extract_casper_from_real_isos() {
    let isos = repo_isos();
    if isos.is_empty() {
        eprintln!(
            "[SKIP] ISO 仓为空（{}）——放入 ubuntu-*-live-server-*.iso 后重跑",
            repo_isos_dir().display()
        );
        return;
    }
    let mut checked = 0usize;
    for (path, entry) in isos {
        eprintln!("[ISO] {} ({})", entry.file, path.display());
        for (inner, min_mb) in [(CASPER_VMLINUZ, 5u64), (CASPER_INITRD, 30u64)] {
            let mut f = fs::File::open(&path).expect("打开 ISO");
            let mut sink = fs::File::create(std::env::temp_dir().join(format!(
                "netboot-extract-{}-{}",
                entry.id,
                inner.replace('/', "_")
            )))
            .expect("创建输出文件");
            let n = iso9660::extract_file(&mut f, inner, &mut sink)
                .unwrap_or_else(|e| panic!("提取 {inner} 失败: {e}"));
            // 体积量级：vmlinuz ≥5MB、initrd ≥30MB（26.04 实测 ~15MB/~120MB 级）
            assert!(
                n >= min_mb * 1024 * 1024,
                "{inner} 体积 {n} 字节低于量级预期 {min_mb}MB"
            );
            // 幻数粗检（读回前 8 字节）
            let mut head = [0u8; 8];
            let mut out = fs::File::open(std::env::temp_dir().join(format!(
                "netboot-extract-{}-{}",
                entry.id,
                inner.replace('/', "_")
            )))
            .unwrap();
            out.read_exact(&mut head).unwrap();
            let ok = match entry.arch.as_str() {
                "amd64" if inner == CASPER_VMLINUZ => &head[..2] == b"MZ", // bzImage EFI stub
                "arm64" if inner == CASPER_VMLINUZ => {
                    // arm64 Ubuntu 内核 = PE/COFF（EFI stub，MZ 头）；裸 Image 形态
                    // 才是 "ARMd"——两种都认
                    &head[..2] == b"MZ" || &head[..4] == b"\x41\x52\x4d\x64"
                }
                _ if inner == CASPER_INITRD => {
                    &head[..2] == b"\x1f\x8b" // gzip
                        || &head[..4] == b"\x28\xb5\x2f\xfd" // zstd
                        || &head[..4] == b"\x02\x21\x4c\x18" // lz4 legacy
                        || &head[..3] == b"\x5d\x00\x00" // xz
                        || &head[..6] == b"070701" || &head[..6] == b"070702" // cpio(newc) 前缀
                }
                _ => true,
            };
            assert!(ok, "{inner} 幻数异常: {head:02x?}");
            eprintln!("      {inner}: {n} 字节, head={head:02x?}",);
            checked += 1;
        }
    }
    assert!(checked >= 2, "至少应校验一件 vmlinuz + 一件 initrd");
}

/// B2：仓清单解析 + 最新稳定版标记（真实仓的文件名集）。
#[test]
#[ignore = "真实仓：同 B1（仓为空优雅跳过）"]
fn b2_repo_entries_and_latest() {
    let isos = repo_isos();
    if isos.is_empty() {
        eprintln!("[SKIP] ISO 仓为空");
        return;
    }
    let mut entries: Vec<_> = isos.iter().map(|(_, e)| e.clone()).collect();
    mark_latest_stable(&mut entries);
    assert!(
        entries.iter().any(|e| e.latest && e.arch == "amd64"),
        "amd64 应有最新稳定版标记"
    );
    assert!(
        entries.iter().any(|e| e.latest && e.arch == "arm64"),
        "arm64 应有最新稳定版标记"
    );
    for e in &entries {
        eprintln!("[{}] {} latest={}", e.arch, e.file, e.latest);
    }
    // 常量口径不漂移
    assert_eq!(netboot::NETBOOT_API_PORT, 8558);
}
