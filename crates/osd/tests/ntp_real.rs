//! osd `ChronyNtp` 真实 chrony 编排测（本机 chronyd/chronyc 跑通验证）。
//!
//! 对应 docs/SANDBOX.md「应入沙箱测试清单」的 chrony/NTP 项。本测**只读为主**：
//! 不改系统时间、不写 `/etc/chrony/chrony.conf`（规格书 §9 红线）。验证点：
//!
//! 1. 真实 `chronyc tracking` stdout 能被 [`parse_tracking`] 解析成结构化字段
//!    （`Stratum`/`Leap status` 等），且 [`ChronyNtp::status`] 返回的 [`NtpStatus`]
//!    不 panic、`offset_ms` 落合理范围。
//! 2. 真实 `chronyc sources` stdout（额外校验源表存在）能被 chronyc 自身正常产出
//!    （侧证守护进程在线、解析器输入来源真实）。
//! 3. `set_servers` 的命令构造（`rewrite_conf_servers` 纯函数）对真实 `chrony.conf`
//!    内容行为正确——**仅 dry-run**：读真实 conf → 内存重写 → 断言产物，**不写回**。
//! 4. `chronyc` 命令存在性探测：二进制在 `$PATH`、能产 exit 0 的 `chronyc tracking`。
//!
//! ## 跑法
//! ```bash
//! cargo build -p osd --features mock
//! sudo cargo test -p osd --features mock --test ntp_real -- --ignored --nocapture
//! ```
//! 非 root / 无 chronyc / chronyd 未运行：**优雅跳过**（eprintln 报告缺什么，不 panic），
//! 不污染默认 `cargo test` 套件（`#[ignore]` 默认不执行）。
//!
//! ## 红线（规格书 §9）
//! - 只读为主：`status`/`read_conf_servers`/`sources` 都不修改系统。
//! - `set_servers` 验证仅做 dry-run（内存重写，不落盘、不 reload）。
//! - **绝不** `makestep`（会真改系统时间）——破坏性操作留给人工运维。

#![cfg(feature = "mock")] // 与 real_zfs_ops.rs 等沙箱测一致：mock feature 下编译

use osd::{
    parse_conf_servers, parse_tracking, rewrite_conf_servers, ChronyNtp, ChronyRunner, NtpManager,
    NtpRunner, TrackingParsed, TRACKING_SAMPLE,
};
use std::process::Command;

/// 纯 Rust 的 `which`：扫 $PATH 找可执行文件（避免引入 which crate 依赖）。
fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 真实环境预检：chronyc 二进制在 + 能产 exit 0 的 `chronyc tracking`（侧证 chronyd 在线）。
///
/// 全部满足返回 true；缺其一则 eprintln 报告缺什么并返回 false（调用方据此优雅跳过）。
/// 注意：**不要求 root**——`chronyc tracking` 非 root 可读，这是本测设计的核心
/// （只读路径不需特权，规格书 §6 硬阻塞只针对 makestep/write_conf）。
fn real_chrony_ready() -> bool {
    if which("chronyc").is_none() {
        eprintln!(
            "[ntp_real] SKIP: `chronyc` 二进制不在 $PATH —— 需装 chrony \
             (Debian/Ubuntu: `apt install chrony`)。"
        );
        return false;
    }
    // `chronyc tracking` exit 0 表示 chronyd 守护进程在线且响应。
    // 非 0 通常是 chronyd 未启动（506 Cannot talk to daemon）。
    let probe = Command::new("chronyc").arg("tracking").output();
    match probe {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            eprintln!(
                "[ntp_real] SKIP: `chronyc tracking` 退出码非 0（chronyd 未运行？）。\
                 stderr: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            false
        }
        Err(e) => {
            eprintln!("[ntp_real] SKIP: spawn `chronyc tracking` 失败：{e}");
            false
        }
    }
}

/// 是否以 root 运行（用于决定 makestep 类特权路径是否可达）。
fn is_root() -> bool {
    Command::new("id").arg("-u").output().ok().and_then(|o| {
        String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse::<u32>()
            .ok()
    }) == Some(0)
}

// ============================================================================
// 真实测（全部 #[ignore]，默认套件不跑）
// ============================================================================

/// a. `sync_status` 真实读：跑真实 ChronyRunner（`chronyc tracking`），断言
///    [`parse_tracking`] 能解析真实输出，且 [`ChronyNtp::status`] 不 panic、
///    `offset_ms` 落合理范围（< 1 小时 sanity）。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "需本机 chronyc + chronyd（只读，不真改系统时间）。跑法：sudo cargo test -p osd --features mock --test ntp_real -- --ignored --nocapture"]
async fn real_sync_status_reads_chronyc_tracking() {
    if !real_chrony_ready() {
        return;
    }

    // 1. 直接调真实 ChronyRunner::tracking()，拿真实 stdout
    let runner = ChronyRunner::new();
    let stdout = runner.tracking().expect("chronyc tracking 应成功");
    eprintln!("[ntp_real] 真实 chronyc tracking stdout:\n{stdout}");

    // 2. 纯函数解析：真实输出必须有 Stratum + Leap status 字段（chrony 文档固定）
    let parsed: TrackingParsed = parse_tracking(&stdout);
    eprintln!(
        "[ntp_real] 解析结果：stratum={} leap={:?} sys_offset={:.6}s last_offset={:.6}s",
        parsed.stratum, parsed.leap_status, parsed.system_offset_sec, parsed.last_offset_sec
    );
    // 真实 chronyc tracking 输出必含 Leap status 行（非空），Stratum 是数字（>=0）。
    // 不强断 leap=="Normal"（本机可能未同步）；但字段必须被解析到（非空 leap）。
    assert!(
        !parsed.leap_status.is_empty(),
        "Leap status 字段必须被解析（真实输出必含此行）"
    );

    // 3. 走 ChronyNtp::status() 全链：tracking + read_conf_servers → NtpStatus
    let ntp = ChronyNtp::new();
    let status = ntp.status().await;
    eprintln!(
        "[ntp_real] ChronyNtp::status() → synced={} offset_ms={}ms servers={:?}",
        status.synced, status.offset_ms, status.servers
    );
    // sanity：偏移绝对值 < 1 小时（3_600_000 ms）。真实已同步机器通常 < 1s；
    // 未同步时 chronyc tracking 仍可能返回（offset 反映当前估算），不应离谱到小时级。
    assert!(
        status.offset_ms.abs() < 3_600_000,
        "offset_ms 应在合理范围（< 1h），实际：{}ms",
        status.offset_ms
    );
}

/// b. `get_status` 真实读：额外验证 `chronyc sources` 也能被 chronyc 正常产出
///    （侧证守护进程在线、源表健康），并验证 read_conf_servers 对真实 conf 行为合理。
///
/// 注：本机 chrony.conf 用 `sourcedir` 指令（Ubuntu 默认），server/pool 在
/// `/etc/chrony/sources.d/*.sources`；故 read_conf_servers 对主 conf 可能返回空——
/// 这是已知部署差异，不算 bug（解析器只认 server/pool 行，sourcedir 是 chrony 扩展）。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "需本机 chronyc + chronyd（只读）。跑法：sudo cargo test -p osd --features mock --test ntp_real -- --ignored --nocapture"]
async fn real_get_status_sources_and_conf() {
    if !real_chrony_ready() {
        return;
    }

    // 1. chronyc sources exit 0（守护进程在线 + 源表产出）
    let src_out = Command::new("chronyc")
        .arg("sources")
        .output()
        .expect("spawn chronyc sources 失败");
    assert!(
        src_out.status.success(),
        "chronyc sources 应 exit 0，实际 {:?} stderr: {}",
        src_out.status.code(),
        String::from_utf8_lossy(&src_out.stderr)
    );
    let src_stdout = String::from_utf8_lossy(&src_out.stdout);
    eprintln!("[ntp_real] 真实 chronyc sources:\n{src_stdout}");
    // sources 表头必含 "Name/IP" 列名（chrony 5.x 固定格式）
    assert!(
        src_stdout.contains("Name/IP") || src_stdout.contains("Name"),
        "chronyc sources 应含源表头"
    );

    // 2. 真实 read_conf_servers（读 /etc/chrony/chrony.conf）：
    //    - conf 存在：返回 Vec（可能空，因 Ubuntu 用 sourcedir）
    //    - conf 不存在：返回 Err
    let runner = ChronyRunner::new();
    match runner.read_conf_servers() {
        Ok(servers) => {
            eprintln!(
                "[ntp_real] read_conf_servers({}) → {servers:?}",
                runner.conf_path()
            );
            // 不强断非空（Ubuntu 默认 conf 用 sourcedir，主 conf 无 server 行是正常的）。
            // 只验证返回类型正确（Vec<String>，不 panic）。
        }
        Err(e) => {
            // conf 不存在（如某些发行版路径不同）→ 不算失败，记录跳过
            eprintln!(
                "[ntp_real] read_conf_servers 失败（conf 不存在或不可读？路径={}）：{e}",
                runner.conf_path()
            );
        }
    }

    // 3. 走 ChronyNtp::status() 全链一次（与 test a 互补，验证 sources 路径下也不 panic）
    let ntp = ChronyNtp::new();
    let status = ntp.status().await;
    eprintln!(
        "[ntp_real] status（sources 路径）→ synced={} offset_ms={}ms",
        status.synced, status.offset_ms
    );
}

/// c. `set_servers` 命令构造验证（**dry-run**，不落盘）：读真实 conf → 内存重写 →
///    断言产物结构正确。**严禁**写回真实 conf / reload（红线）。
///
/// 用真实 `/etc/chrony/chrony.conf` 作为输入验证 [`rewrite_conf_servers`]：
/// - 重写后所有原 server/pool 行被删除。
/// - 新 server 行追加（带 iburst）。
/// - 非 server/pool 行（driftfile/rtcsync/makestep/sourcedir 等）保留。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "需本机 chrony.conf（dry-run 不落盘，不 reload）。跑法：sudo cargo test -p osd --features mock --test ntp_real -- --ignored --nocapture"]
async fn real_set_servers_dry_run_command_construction() {
    if which("chronyc").is_none() {
        eprintln!("[ntp_real] SKIP: 无 chronyc（不强制需要，但本测聚焦真实 conf）");
        return;
    }

    // 读真实 conf（只读）。若不存在则跳过（不算失败）。
    let conf_path = "/etc/chrony/chrony.conf";
    let real_conf = match std::fs::read_to_string(conf_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[ntp_real] SKIP: 读真实 {conf_path} 失败（{e}）——本测需真实 conf 作输入");
            return;
        }
    };
    eprintln!(
        "[ntp_real] 真实 conf 长度={} 字节，前 200 字节：\n{}",
        real_conf.len(),
        real_conf.chars().take(200).collect::<String>()
    );

    // 解析现有 servers（dry-run 输入侧验证）
    let before = parse_conf_servers(&real_conf);
    eprintln!("[ntp_real] 现有 server/pool 行解析：{before:?}");

    // dry-run 重写：内存操作，**不写回**（红线：不改真实 conf）
    let new_servers: Vec<String> = vec!["0.pool.ntp.org".into(), "1.pool.ntp.org".into()];
    let rewritten = rewrite_conf_servers(&real_conf, &new_servers);

    // 断言 1：新 server 行追加（带 iburst）
    assert!(
        rewritten.contains("server 0.pool.ntp.org iburst"),
        "重写后应含新 server 行（实际片段：{}）",
        rewritten
            .lines()
            .filter(|l| l.contains("pool.ntp.org"))
            .take(3)
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        rewritten.contains("server 1.pool.ntp.org iburst"),
        "重写后应含第二个新 server 行"
    );

    // 断言 2：原 server/pool 行被删除（before 中的主机不应再作为 server/pool 行首字段）
    //   注：若原 conf 用 sourcedir（Ubuntu 默认），before 为空，此断言空过。
    for old in &before {
        let old_server_line = format!("server {old}");
        let old_pool_line = format!("pool {old}");
        assert!(
            !rewritten.contains(&old_server_line),
            "旧 server 行 {old_server_line} 应被删除"
        );
        assert!(
            !rewritten.contains(&old_pool_line),
            "旧 pool 行 {old_pool_line} 应被删除"
        );
    }

    // 断言 3：非 server/pool 配置保留（sourcedir/driftfile/rtcsync/makestep 等关键指令）
    //   抽取真实 conf 中所有非 server/pool 非空非注释行的首字段，验证它们在重写产物中仍在。
    let preserved_keywords: Vec<String> = real_conf
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .filter_map(|l| l.split_whitespace().next().map(|s| s.to_string()))
        .filter(|kw| kw != "server" && kw != "pool")
        .collect();
    for kw in &preserved_keywords {
        assert!(
            rewritten.contains(kw.as_str()),
            "非 server/pool 关键字 {kw} 应被保留"
        );
    }
    eprintln!(
        "[ntp_real] dry-run 重写 OK：保留关键字 {:?}，新 servers {new_servers:?}（未落盘）",
        preserved_keywords.iter().take(5).collect::<Vec<_>>()
    );

    // 旁证：rewrite_conf_servers 是纯函数（无 IO），用 TRACKING_SAMPLE 同理不触发任何
    // 系统变更。这里额外验证 fixture 样本仍往返正确（回归）。
    let fixture_rewritten = rewrite_conf_servers("server old.x\nrtcsync\n", &["new.x".into()]);
    assert!(fixture_rewritten.contains("server new.x iburst"));
    assert!(!fixture_rewritten.contains("server old.x"));
    assert!(fixture_rewritten.contains("rtcsync"));
    let _ = TRACKING_SAMPLE; // 确认 re-export 可见
}

/// d. `chronyc` 命令存在性探测 + 真实输出可被现有解析器解析。
///    本测是 a/b 的前置依赖探针：若本测 SKIP，a/b 也会 SKIP。
#[tokio::test(flavor = "multi_thread")]
#[ignore = "需本机 chronyc。跑法：cargo test -p osd --features mock --test ntp_real -- --ignored --nocapture（非 root 也可）"]
async fn real_chronyc_binary_probe_and_parser_compat() {
    // 1. 二进制存在
    let Some(path) = which("chronyc") else {
        eprintln!("[ntp_real] SKIP: chronyc 不在 $PATH");
        return;
    };
    eprintln!("[ntp_real] chronyc 路径：{}", path.display());

    // 2. `chronyc tracking` exit 0（守护进程响应）
    let out = Command::new("chronyc")
        .arg("tracking")
        .output()
        .expect("spawn chronyc tracking 失败");
    if !out.status.success() {
        eprintln!(
            "[ntp_real] SKIP: chronyc tracking 非 0（chronyd 未运行？exit={:?}）",
            out.status.code()
        );
        return;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    eprintln!(
        "[ntp_real] chronyc tracking 真实输出长度={} 字节",
        stdout.len()
    );

    // 3. 真实输出必须能被 parse_tracking 解析（不 panic、字段类型正确）
    let parsed = parse_tracking(&stdout);
    // 真实 chronyc tracking 必含 Leap status 行 → 解析后非空
    assert!(
        !parsed.leap_status.is_empty(),
        "真实输出应被解析出 Leap status（实际 parsed={parsed:?}）"
    );
    // stratum 合法范围 [0, 16]（NTP 协议：0=未同步，16=不可达）
    assert!(
        parsed.stratum <= 16,
        "stratum 应在 NTP 合法范围 [0,16]，实际 {}",
        parsed.stratum
    );
    eprintln!(
        "[ntp_real] chronyc 输出解析 OK：stratum={} leap={:?} offset={:.6}s",
        parsed.stratum, parsed.leap_status, parsed.system_offset_sec
    );

    // 4. 额外：chronyc -v 版本探测（记录 chrony 版本，便于排查解析兼容性）
    let ver = Command::new("chronyc").arg("-v").output();
    if let Ok(v) = ver {
        eprintln!(
            "[ntp_real] chronyc 版本：{}",
            String::from_utf8_lossy(&v.stdout).trim()
        );
    }

    // 5. 额外：若以 root 跑，验证 makestep 特权路径"命令构造"可达（不真改时间——
    //    本测**不**调 makestep，只验证 ChronyRunner 实例化 + trait 方法可见）。
    //    这里的意图是证明 root 下 set_servers 路径的 runner 构造无误，破坏性操作
    //    留给人工运维（红线）。
    if is_root() {
        let runner = ChronyRunner::new();
        // 只验证 conf_path 可读路径（不触发写）：
        eprintln!(
            "[ntp_real] root 下 ChronyRunner conf_path={}",
            runner.conf_path()
        );
        // 不调 makestep / write_conf_servers（红线：不改时间/conf）。
    } else {
        eprintln!("[ntp_real] 非 root：跳过特权路径探针（仅记录，不算失败）");
    }
}
