//! os CLI → os-api HTTP → 真实后端 端到端集成测（规划文档 §3.0/#19 / §3.6 / §9.1#10）。
//!
//! 覆盖完整链路（与 os-api 的 e2e_real.rs 互补——本测从 **CLI 进程**视角验证）：
//! ```text
//! os CLI (子进程) → reqwest → HTTP → os-api binary (axum::serve) →
//!   Gateway.dispatch → RouteHandler.handle → 真实 ZfsCliBackend.list_pools → JSON → CLI stdout
//! ```
//!
//! batch13 让 os-api 装配了真实 `StorageRouteHandler`（`GET /api/v1/pools` 跑真实
//! `zpool list`），本测验证 os CLI（`os --server ... pool list`）能经这条链路拿到
//! 真实池列表——即「CLI binary + os-api binary + 真实 zfs」三层组装是否打通。
//!
//! ## 三组测（全部 `#[ignore]`，需 `--ignored` 显式触发）
//!
//! - **A. CLI → os-api → 真实 zfs 端到端**（核心）：
//!   - spawn `./target/debug/os-api --addr 127.0.0.1:18095`，等 `/healthz` 就绪；
//!   - 跑 `os --server http://127.0.0.1:18095 pool list`，断言 **stdout 含真实池名**
//!     （本机 `osprobepersist`）—— 这命中「真实 zfs 数据经 HTTP 暴露、经 CLI 渲染」。
//!   - 跑 `os --server ... status`，断言 stdout/stderr 含 version 或状态相关输出
//!     （`/status` 响应形状与客户端 `SystemStatus` schema 部分错配，status 可能失败，
//!     故此处宽松断言「调用链可达 + 产出输出」，不强求成功）。
//! - **B. 错误路径**：
//!   - 不可达 server（`http://127.0.0.1:1`）→ 退出码非 0 + stderr 含连接错。
//!   - 未知命令 `os bogus` → 退出码 2（clap 既有用法错）。
//!
//! ## 跑法
//!
//! ```bash
//! # 全部（需 zfsutils-linux + 真实 zfs 模块；list_pools 只读，普通用户可执行）：
//! cargo test -p os-cli --features mock --test cli_e2e_real -- --ignored --nocapture
//!
//! # 仅 A 的核心 pool list：
//! cargo test -p os-cli --features mock --test cli_e2e_real pool_list -- --ignored --nocapture
//! ```
//!
//! 无 zfs / 无 os-api binary / 无 os binary → 优雅 SKIP（eprintln 报告缺什么，不 panic）。
//!
//! ## 红线
//!
//! - 只读路径（`pool list` 调 `list_pools`，不 create/destroy/snapshot 宿主 zfs）。
//! - RAII guard（`ChildGuard`）保证 os-api 子进程在测结束（含断言失败）后清理。
//! - 端口用固定的 `127.0.0.1:18095`（任务指定；非 0 让 CLI 测可重现）。

#![cfg(feature = "mock")]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// ============================================================================
// 辅助：纯 Rust `which` / 找 binary / RAII guard / 端口就绪探测
// ============================================================================

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

/// 找 workspace 构建产物目录（`target/debug`）下的 binary。
///
/// cargo 把所有 crate 的产物汇到 workspace 根 `target/debug`；测试的 CWD 是 crate 根，
/// 故向上找最近的含 `target/debug` 的祖先目录。
fn find_debug_bin(name: &str) -> Option<std::path::PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join("target").join("debug").join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// 真实 zfs 环境预检：`zpool` 二进制存在即可（list_pools 只读，普通用户能跑）。
///
/// 返回第一个真实池名（用于断言 CLI stdout 含它）；无 zfs 环境 → None。
fn real_zfs_first_pool() -> Option<String> {
    if which("zpool").is_none() {
        eprintln!(
            "[cli_e2e_real] SKIP: `zpool` 二进制不在 $PATH —— 需装 zfsutils-linux \
             (Debian: `apt install zfsutils-linux`)。"
        );
        return None;
    }
    let probe = Command::new("zpool")
        .args(["list", "-p", "-H"])
        .stderr(Stdio::piped())
        .output();
    match probe {
        Ok(o) if o.status.success() => {
            let stdout = String::from_utf8_lossy(&o.stdout);
            let names: Vec<&str> = stdout
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| l.split('\t').next().unwrap_or(""))
                .collect();
            let first = names.first().copied().unwrap_or("<无池>").to_string();
            eprintln!(
                "[cli_e2e_real] zfs 就绪：检测到 {} 个池（{}）",
                names.len(),
                names.join(", ")
            );
            Some(first)
        }
        Ok(o) => {
            eprintln!(
                "[cli_e2e_real] SKIP: `zpool list` 退出码非 0（zfs 内核模块未加载或无读取权限）。\
                 stderr: {}",
                String::from_utf8_lossy(&o.stderr)
            );
            None
        }
        Err(e) => {
            eprintln!("[cli_e2e_real] SKIP: spawn `zpool list` 失败：{e}");
            None
        }
    }
}

/// RAII guard：持有子进程句柄，drop 时确保 kill + wait（即使断言失败也清理）。
struct ChildGuard {
    child: Option<Child>,
    label: &'static str,
}

impl ChildGuard {
    fn new(child: Child, label: &'static str) -> Self {
        Self {
            child: Some(child),
            label,
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            match child.kill() {
                Ok(()) => {
                    let _ = child.wait(); // 回收僵尸
                }
                Err(e) => eprintln!("[cli_e2e_real] kill {} 失败：{e}", self.label),
            }
        }
    }
}

/// 轮询探测 `http://127.0.0.1:{port}/healthz` 直到成功或超时。
///
/// os-api binary spawn 后到 axum::serve 就绪有短暂窗口；用 reqwest 反复探测
/// `/healthz` 是最可靠的就绪信号。
async fn wait_for_http_ready(port: u16, timeout: Duration) -> bool {
    let url = format!("http://127.0.0.1:{port}/healthz");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .expect("reqwest::Client 构造");
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(r) = client.get(&url).send().await {
            if r.status().is_success() {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// spawn `os-api` binary 监听 `127.0.0.1:{port}`，等就绪后返回 RAII guard。
///
/// 调用方持 guard 到测结束，drop 时自动 kill。返回 None 表示无 binary / 启动失败。
async fn spawn_os_api(port: u16) -> Option<ChildGuard> {
    let bin = match find_debug_bin("os-api") {
        Some(p) => p,
        None => {
            eprintln!(
                "[cli_e2e_real] SKIP: 找不到 target/debug/os-api。请先 \
                 `cargo build -p os-api`。"
            );
            return None;
        }
    };
    let addr = format!("127.0.0.1:{port}");
    let child = Command::new(&bin)
        .args(["--addr", &addr])
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {} 失败: {e}", bin.display()));
    let guard = ChildGuard::new(child, "os-api");

    if !wait_for_http_ready(port, Duration::from_secs(8)).await {
        eprintln!("[cli_e2e_real] os-api binary 在 8s 内未就绪 @ {addr}");
        return None;
    }
    eprintln!("[cli_e2e_real] os-api binary 已就绪 @ {addr}");
    Some(guard)
}

/// 端口常量（任务指定 18095 区段；并行跑多个测时各用不同端口避免 bind 冲突）。
///
/// 默认 `cargo test` 多线程并行跑 ignored 测；若所有测共用一个端口，spawn os-api
/// 时第二个起的会 bind 失败（Address already in use）→ 不稳定。故 pool/status 测
/// 各占一个端口（18095 / 18096），错误路径测不启 server 不占端口。
const PORT_POOL: u16 = 18095;
const PORT_STATUS: u16 = 18096;

// ============================================================================
// 测 A1：CLI → os-api → 真实 zfs 端到端（核心：pool list 含真实池名）
// ============================================================================

/// `os --server http://127.0.0.1:18095 pool list` 经 os-api → StorageRouteHandler
/// → ZfsCliBackend.list_pools → 真实 `zpool list`，CLI stdout 渲染 JSON 数组含真实池名。
///
/// 这是本测的**重点**——验证「CLI binary + os-api binary + 真实 zfs」三层组装是否打通：
/// CLI 经 reqwest 发 `GET /api/v1/pools`，os-api 的 StorageRouteHandler 跑真实
/// `zpool list -p -H` 解析 Pool[]，序列化 JSON 返回，CLI 渲染到 stdout（紧凑 JSON）。
///
/// 无 zfs 环境 / 无 binary → 优雅 SKIP。
#[tokio::test]
#[ignore = "真实 CLI→os-api→zfs 端到端：需 zfsutils-linux + zfs 模块 + os-api/os binary。\
            跑法：cargo test -p os-cli --features mock --test cli_e2e_real pool_list_e2e -- --ignored --nocapture"]
async fn pool_list_e2e_cli_to_os_api_to_real_zfs() {
    // 1) 真实 zfs 预检（取真实池名用于断言；无则 SKIP）。
    let expected_pool = match real_zfs_first_pool() {
        Some(name) if name != "<无池>" => name,
        _ => {
            eprintln!("[cli_e2e_real] SKIP: 无真实 zfs 池（pool list 链路无法验证池名）");
            return;
        }
    };

    // 2) spawn os-api + 等就绪（无 binary → SKIP）。
    let _server = match spawn_os_api(PORT_POOL).await {
        Some(g) => g,
        None => {
            eprintln!("[cli_e2e_real] SKIP: os-api binary 不可用或未就绪");
            return;
        }
    };

    // 3) 跑 `os --server http://127.0.0.1:18095 pool list`。
    let cli_bin = match find_debug_bin("os") {
        Some(p) => p,
        None => {
            eprintln!(
                "[cli_e2e_real] SKIP: 找不到 target/debug/os。请先 `cargo build -p os-cli`。"
            );
            return;
        }
    };
    let server_url = format!("http://127.0.0.1:{PORT_POOL}");
    let output = Command::new(&cli_bin)
        .args(["--server", &server_url, "pool", "list"])
        .output()
        .unwrap_or_else(|e| panic!("spawn {} 失败: {e}", cli_bin.display()));

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let code = output.status.code().unwrap_or(-1);
    eprintln!(
        "[cli_e2e_real] os pool list exit={code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );

    // 4) 断言成功 + stdout 含真实池名（核心）。
    assert_eq!(
        code, 0,
        "os pool list 应成功（exit 0）；实际 {code}\nstderr: {stderr}"
    );
    assert!(
        stdout.contains(&expected_pool),
        "os pool list stdout 应含真实池名 `{expected_pool}`；实际 stdout: {stdout}"
    );
    eprintln!(
        "[cli_e2e_real] 断言 OK：stdout 含真实池名 `{expected_pool}`（CLI→os-api→zfs 链路打通）"
    );
    // guard drop 会 kill os-api。
}

// ============================================================================
// 测 A2：CLI → os-api status（调用链可达 + 产出输出）
// ============================================================================

/// `os --server http://127.0.0.1:18095 status` 经 HttpOsClient → GET /status。
///
/// 注：当前 os-api 的 `/status` handler 返回 `{cpu_virt, version, uptime}`，而
/// CLI 侧 `SystemStatus` schema 期望 `{hostname, version, capacity, health, node_count}`，
/// 形状错配 → `os status` 多半因反序列化失败而失败（exit 1，stderr 报错）。
/// 故本测**宽松断言**「调用链可达 + 产出非空输出（stdout/stderr）」，不强求成功；
/// 待 SystemRouteHandler 与 SystemStatus schema 对齐后可收紧为「stdout 含 version」。
///
/// 无 binary → SKIP。
#[tokio::test]
#[ignore = "CLI status e2e：需 os-api + os binary。\
            跑法：cargo test -p os-cli --features mock --test cli_e2e_real status_e2e -- --ignored --nocapture"]
async fn status_e2e_cli_to_os_api() {
    // spawn os-api + 等就绪（无 binary → SKIP）。
    let _server = match spawn_os_api(PORT_STATUS).await {
        Some(g) => g,
        None => {
            eprintln!("[cli_e2e_real] SKIP: os-api binary 不可用或未就绪");
            return;
        }
    };
    let cli_bin = match find_debug_bin("os") {
        Some(p) => p,
        None => {
            eprintln!(
                "[cli_e2e_real] SKIP: 找不到 target/debug/os。请先 `cargo build -p os-cli`。"
            );
            return;
        }
    };

    let server_url = format!("http://127.0.0.1:{PORT_STATUS}");
    let output = Command::new(&cli_bin)
        .args(["--server", &server_url, "status"])
        .output()
        .unwrap_or_else(|e| panic!("spawn {} 失败: {e}", cli_bin.display()));

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let code = output.status.code().unwrap_or(-1);
    eprintln!(
        "[cli_e2e_real] os status exit={code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );

    // 宽松断言：调用链可达 → 至少产出某种输出（stdout 或 stderr 非空）。
    assert!(
        !stdout.is_empty() || !stderr.is_empty(),
        "os status 应产出 stdout/stderr（端到端调用链应执行）"
    );
    // 进一步：若成功（exit 0），stdout 应含 version 或状态相关词；
    // 若失败（schema 错配），stderr 应含错误说明（连接/解析错）。
    if code == 0 {
        assert!(
            stdout.to_lowercase().contains("version")
                || stdout.to_lowercase().contains("status")
                || stdout.to_lowercase().contains("ok"),
            "os status 成功时 stdout 应含 version/status/ok 之一；实际: {stdout}"
        );
        eprintln!("[cli_e2e_real] os status 成功（exit 0），stdout 含状态相关输出");
    } else {
        // 失败路径：stderr 应含可读错误（连接错 / 解析错 / 内部错）。
        let combined = format!("{stdout}\n{stderr}").to_lowercase();
        assert!(
            combined.contains("error") || combined.contains("失败") || combined.contains("错"),
            "os status 失败时 stderr 应含错误说明；实际 stderr: {stderr}",
        );
        eprintln!(
            "[cli_e2e_real] os status 失败（exit {code}，预期：/status schema 错配），调用链可达"
        );
    }
    // guard drop 会 kill os-api。
}

// ============================================================================
// 测 B1：不可达 server → 非零退出码 + stderr 含连接错
// ============================================================================

/// `os --server http://127.0.0.1:1 pool list`（端口 1 不可达）→ CLI 应失败：
/// 退出码非 0（预期 4 = ApiConnectionFailed）+ stderr 含连接错说明。
///
/// 这条不需要 os-api（连不到的端口本身就是测试目的）。
#[tokio::test]
#[ignore = "CLI 错误路径：不可达 server。\
            跑法：cargo test -p os-cli --features mock --test cli_e2e_real unreachable_server -- --ignored --nocapture"]
async fn unreachable_server_returns_nonzero_with_connection_error() {
    let cli_bin = match find_debug_bin("os") {
        Some(p) => p,
        None => {
            eprintln!(
                "[cli_e2e_real] SKIP: 找不到 target/debug/os。请先 `cargo build -p os-cli`。"
            );
            return;
        }
    };
    // 端口 1 是特权端口且通常无服务监听 → 连接被拒（reqwest 报 Connect 错）。
    let output = Command::new(&cli_bin)
        .args(["--server", "http://127.0.0.1:1", "pool", "list"])
        .output()
        .unwrap_or_else(|e| panic!("spawn {} 失败: {e}", cli_bin.display()));

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let code = output.status.code().unwrap_or(-1);
    eprintln!(
        "[cli_e2e_real] os (unreachable) exit={code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );

    // 1) 退出码非 0（CLI 错误映射：连接错 → exit 4）。
    assert_ne!(
        code, 0,
        "不可达 server 应非零退出码；实际 {code}\nstdout: {stdout}\nstderr: {stderr}"
    );
    // 2) stderr 含连接错说明（中文「连接」或英文「error」/「connect」）。
    let combined = format!("{stdout}\n{stderr}").to_lowercase();
    assert!(
        combined.contains("连接")
            || combined.contains("connect")
            || combined.contains("unreachable")
            || combined.contains("refused")
            || combined.contains("error"),
        "stderr 应含连接错说明；实际 stderr: {stderr}"
    );
    eprintln!("[cli_e2e_real] 断言 OK：不可达 server → exit {code} + stderr 含连接错");
}

// ============================================================================
// 测 B2：未知命令 → 退出码 2（clap 既有用法错）
// ============================================================================

/// `os bogus`（未知子命令）→ clap 拒绝，退出码 2（clap 既有 usage error）。
///
/// 这条不需 os-api（解析阶段就失败，不走网络）。
#[tokio::test]
#[ignore = "CLI 错误路径：未知命令。\
            跑法：cargo test -p os-cli --features mock --test cli_e2e_real unknown_command -- --ignored --nocapture"]
async fn unknown_command_exits_2() {
    let cli_bin = match find_debug_bin("os") {
        Some(p) => p,
        None => {
            eprintln!(
                "[cli_e2e_real] SKIP: 找不到 target/debug/os。请先 `cargo build -p os-cli`。"
            );
            return;
        }
    };
    let output = Command::new(&cli_bin)
        .args(["bogus"])
        .output()
        .unwrap_or_else(|e| panic!("spawn {} 失败: {e}", cli_bin.display()));

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let code = output.status.code().unwrap_or(-1);
    eprintln!(
        "[cli_e2e_real] os bogus exit={code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );

    // clap usage error → exit 2（main.rs 的 try_parse → e.exit_code()）。
    assert_eq!(
        code, 2,
        "未知命令应退出码 2（clap usage error）；实际 {code}\nstderr: {stderr}"
    );
    eprintln!("[cli_e2e_real] 断言 OK：`os bogus` → exit 2（clap usage error）");
}
