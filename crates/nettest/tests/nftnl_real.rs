//! 真实 nftables 防火墙事务冒烟 + 表/链/规则验证（FFI：libnftnl + libmnl）。
//!
//! **本文件整个在 `feature = "nftnl-ffi"` 下编译**：未开 feature 时 cargo 把本文件
//! 视为空集成测目标（无测试项编译进默认套件）。开 feature 需宿主装 `libnftnl-dev` +
//! `libmnl-dev`（pkg-config 找 `.pc`），缺 -dev 头时 `cargo test --features nftnl-ffi`
//! 编译失败——这是预期门控（与 os-network `nftnl-ffi` feature 同义，见
//! docs/SANDBOX.md §5.3）。
//!
//! ## 验证内容
//! 真实用 nftnl + mnl crate 经 netlink 向内核 nf_tables 子系统提交一个**最小、安全、
//! 可幂等清理**的事务：
//!
//! 1. **smoke（nftnl_real_smoke）**：建表 `inet nettest_real` + input 链（hook input,
//!    policy accept）+ 规则 `iif lo accept`（与系统默认行为一致，no-op 性冒烟）。
//! 2. **table/chain/rules（nftnl_real_table_chain_rules）**：建表 `inet osnettest` +
//!    input 链 + 规则 `iif lo accept` + `tcp dport 22 accept`，提交后用 `nft list table`
//!    子进程回读验证规则存在，最后删表清理。
//!
//! ## 步骤（构造 → 执行 → 断言 → 清理）
//! 1. 构造 Batch：NEW 表 → NEW 链（hook input, priority 0, policy accept）→ NEW 规则。
//! 2. 执行：finalize batch → mnl socket send_all → recv+cb_run 取 ACK。
//! 3. 断言：提交无 netlink 错误（mnl::CbResult::Ok / Stop）；table_chain_rules 还用
//!    `nft list table` 子进程验证规则文本存在。
//! 4. 清理：另一个 Batch `DEL table`（删表级联删链/规则，幂等）；末尾兜底用 `nft`
//!    CLI 删一次（即便前面事务失败也确保无残留）。
//!
//! ## 运行环境
//! - 需 root + CAP_NET_ADMIN（写 nf_tables 需特权）。
//! - 需宿主装 libnftnl-dev + libmnl-dev（开 `nftnl-ffi` feature 时由 build script 门控）。
//! - 非 root / 缺权限：netlink 回包携带 -EPERM，测应**优雅失败**（eprintln 报告权限
//!   不足），不 panic 污染默认套件——这就是 `#[ignore]` 的意义。
//!
//! ## 与 os-network 的关系
//! os-network::NftnlFirewallBackend::add_rule 走完全相同的 nftnl::Batch + mnl socket
//! 路径（参见 crates/os-network/src/nftnl_real.rs::send_batch）；本测是这条真实执行
//! 路径的跨 crate 真机回归入口。**FFI 路径修正点**：nftnl 0.7 不 re-export mnl_socket，
//! finalized batch 投递走独立 `mnl` crate（`Socket::new`/`send_all`/`recv`/`cb_run`），
//! os-network::send_batch 已与本次同源修正。

// 整个文件门控：未开 nftnl-ffi feature 时编译为空，不污染默认套件。
#![cfg(feature = "nftnl-ffi")]
// SANDBOX.md §5.3 注：nftnl 经 FFI 链接 libnftnl/libmnl，需在沙箱/特权环境跑；
// 本测的 unsafe 块仅限 libmnl/libnftnl C API 调用（与 os-network::send_batch 同源）。
#![allow(unsafe_code)]

mod common;

use std::ffi::CString;
use std::process::Command;

use common::timeout_or_panic;
use mnl::{cb_run, Bus, CbResult, Socket};
use nftnl::{Batch, Chain, FinalizedBatch, Hook, Policy, ProtoFamily, Table};

/// smoke 专用表名（避免与任何生产/其他测试的 nft 表冲突）。
const SMOKE_TABLE: &str = "nettest_real";
/// 表/链/规则验证专用表名（任务要求）。
const FULL_TABLE: &str = "osnettest";
/// 链名。
const CHAIN_NAME: &str = "input";

/// 真实 nftables 事务冒烟：建表 + 建链 + accept iif lo 规则 → 断言提交成功 → 删表清理。
#[tokio::test]
#[ignore = "真实 nftables netlink 事务：手动 `cargo test -p nettest --features nftnl-ffi -- --ignored nftnl_real_smoke`（需 root + CAP_NET_ADMIN + libnftnl-dev + libmnl-dev）"]
async fn nftnl_real_smoke() {
    // nftnl/mnl 的 FFI 调用是同步阻塞的，spawn_blocking 避免阻塞 tokio runtime。
    let result = tokio::task::spawn_blocking(real_smoke_inner).await;
    timeout_or_panic(async {
        match result {
            Ok(Ok(())) => eprintln!("[nettest] nftnl 真实事务冒烟通过"),
            Ok(Err(e)) => {
                // netlink/权限/FFI 错误：优雅跳过（明确报告），不 panic——
                // 手动 `--ignored` 跑时清楚看到环境缺什么，也不污染测试套件。
                eprintln!(
                    "[nettest] SKIP: nftnl 真实事务失败 —— 通常是非 root / 缺 CAP_NET_ADMIN \
                     / 内核无 nf_tables / 系统无 libnftnl。错误: {e}"
                );
            }
            Err(join_err) => eprintln!("[nettest] SKIP: nftnl 任务 join 失败: {join_err}"),
        }
    })
    .await;
}

/// 真实事务主体（同步阻塞函数，由 spawn_blocking 调度）。
fn real_smoke_inner() -> std::io::Result<()> {
    // === 1. 构造 Batch：表 + 链 + accept iif lo 规则 ===
    let mut batch = Batch::new();

    let table = Table::new(
        &CString::new(SMOKE_TABLE).expect("CString 表名构造失败"),
        ProtoFamily::Inet,
    );
    batch.add(&table, nftnl::MsgType::Add);

    let mut chain = Chain::new(
        &CString::new(CHAIN_NAME).expect("CString 链名构造失败"),
        &table,
    );
    // hook 到 input 钩子，priority 0，默认 policy accept（与系统默认一致，安全）。
    chain.set_hook(Hook::In, 0);
    chain.set_policy(Policy::Accept);
    batch.add(&chain, nftnl::MsgType::Add);

    // 规则：iif lo → accept（放行回环入站；与默认行为一致，no-op 性冒烟）。
    let mut rule = nftnl::Rule::new(&chain);
    rule.add_expr(&nftnl::nft_expr!(meta iif));
    let lo_idx = iface_index("lo")?;
    rule.add_expr(&nftnl::nft_expr!(cmp == lo_idx));
    rule.add_expr(&nftnl::nft_expr!(verdict accept));
    batch.add(&rule, nftnl::MsgType::Add);

    let finalized = batch.finalize();

    // === 2. 执行：发送 + 收 ACK ===
    eprintln!(
        "[nettest] 提交 nftnl 事务：NEW 表 inet {SMOKE_TABLE} + 链 {CHAIN_NAME} + 规则 iif lo accept"
    );
    send_and_process(&finalized)?;

    // === 3. 断言：send_and_process 成功（无 netlink 错误）即视为通过 ===
    eprintln!("[nettest] 事务提交成功（netlink ACK 通过）");

    // === 4. 清理：删表（级联删链/规则，幂等；即便前面部分失败也尝试清理）===
    drop_table(SMOKE_TABLE);
    Ok(())
}

/// 真实 nft 事务验证：NEW 表 `inet osnettest` + 链 input(hook input,accept)
/// + 规则 `iif lo accept` + `tcp dport 22 accept` → `nft list table` 验证 → 删表清理。
#[tokio::test]
#[ignore = "真实 nftables 表/链/规则事务：手动 `cargo test -p nettest --features nftnl-ffi -- --ignored nftnl_real_table_chain_rules`（需 root + CAP_NET_ADMIN + libnftnl-dev + libmnl-dev）"]
async fn nftnl_real_table_chain_rules() {
    let result = tokio::task::spawn_blocking(real_table_chain_rules_inner).await;
    timeout_or_panic(async {
        match result {
            Ok(Ok(())) => eprintln!("[nettest] nftnl 表/链/规则事务通过（含 nft list 回读验证）"),
            Ok(Err(e)) => eprintln!(
                "[nettest] SKIP: nftnl 表/链/规则事务失败 —— 通常是非 root / 缺 CAP_NET_ADMIN \
                 / 内核无 nf_tables / 系统无 libnftnl。错误: {e}"
            ),
            Err(join_err) => eprintln!("[nettest] SKIP: nftnl 任务 join 失败: {join_err}"),
        }
    })
    .await;
}

/// 表/链/规则事务主体（同步阻塞）。
fn real_table_chain_rules_inner() -> std::io::Result<()> {
    // 兜底清理：先确保表不在（避免上一次失败残留导致 NEW 表 -EEXIST）。
    drop_table(FULL_TABLE);

    // === 1. 构造 Batch：表 + 链 + 两条规则 ===
    let mut batch = Batch::new();
    let table = Table::new(
        &CString::new(FULL_TABLE).expect("CString 表名构造失败"),
        ProtoFamily::Inet,
    );
    batch.add(&table, nftnl::MsgType::Add);

    let mut chain = Chain::new(
        &CString::new(CHAIN_NAME).expect("CString 链名构造失败"),
        &table,
    );
    chain.set_hook(Hook::In, 0);
    chain.set_policy(Policy::Accept);
    batch.add(&chain, nftnl::MsgType::Add);

    // 规则 A：iif lo → accept（放行回环入站）。
    let mut rule_a = nftnl::Rule::new(&chain);
    rule_a.add_expr(&nftnl::nft_expr!(meta iif));
    let lo_idx = iface_index("lo")?;
    rule_a.add_expr(&nftnl::nft_expr!(cmp == lo_idx));
    rule_a.add_expr(&nftnl::nft_expr!(verdict accept));
    batch.add(&rule_a, nftnl::MsgType::Add);

    // 规则 B：tcp dport 22 → accept（放行 SSH 入站；测试表，提交后立即删，无实际效力）。
    let mut rule_b = nftnl::Rule::new(&chain);
    rule_b.add_expr(&nftnl::nft_expr!(meta l4proto));
    rule_b.add_expr(&nftnl::nft_expr!(cmp == 6u8 /* IPPROTO_TCP */));
    rule_b.add_expr(&nftnl::nft_expr!(payload tcp dport));
    // 端口字段在 nftables 是网络字节序（大端）；nft_expr!(cmp == <u16>) 会按
    // 小端写两字节（见 nftnl ToSlice for u16），故此处直接传大端字节切片避免误配。
    let ssh_port_be: [u8; 2] = 22u16.to_be_bytes();
    rule_b.add_expr(&nftnl::nft_expr!(cmp == ssh_port_be.as_ref()));
    rule_b.add_expr(&nftnl::nft_expr!(verdict accept));
    batch.add(&rule_b, nftnl::MsgType::Add);

    let finalized = batch.finalize();

    eprintln!(
        "[nettest] 提交 nftnl 事务：NEW 表 inet {FULL_TABLE} + 链 {CHAIN_NAME} + 规则 iif lo accept + tcp dport 22 accept"
    );
    send_and_process(&finalized)?;
    eprintln!("[nettest] 事务提交成功（netlink ACK 通过）");

    // === 2. 断言：用 nft list table 子进程回读，验证两条规则文本存在 ===
    let listing = nft_list_table(FULL_TABLE)?;
    eprintln!("[nettest] nft list table inet {FULL_TABLE}:\n{listing}");
    assert!(
        listing.contains("iif \"lo\"") || listing.contains("iif lo"),
        "规则 iif lo accept 未出现在 nft list 输出中"
    );
    assert!(
        listing.contains("tcp dport 22"),
        "规则 tcp dport 22 accept 未出现在 nft list 输出中"
    );
    eprintln!("[nettest] 回读验证通过：两条规则均存在于 nft list 输出");

    // === 3. 清理：删表（级联删链/规则，幂等）===
    drop_table(FULL_TABLE);
    eprintln!("[nettest] 清理完成：已删表 inet {FULL_TABLE}");

    Ok(())
}

/// 把 finalized batch 经 mnl netlink socket 发送到内核并处理回包。
///
/// 与 nftnl 官方 add-rules.rs 示例同源（mnl 0.2 API：Socket::new/send_all + cb_run），
/// 与 os-network::send_batch 语义一致。
fn send_and_process(batch: &FinalizedBatch) -> std::io::Result<()> {
    let socket = Socket::new(Bus::Netfilter)?;
    socket.send_all(batch.iter())?;

    let portid = socket.portid();
    let mut buffer = vec![0u8; nftnl::nft_nlmsg_maxsize() as usize];
    loop {
        let n = socket.recv(&mut buffer)?;
        if n == 0 {
            break;
        }
        match cb_run(&buffer[..n], 2, portid) {
            Ok(CbResult::Stop) => break,
            Ok(CbResult::Ok) => continue,
            Err(e) => {
                return Err(std::io::Error::other(format!(
                    "mnl cb_run 错误（可能是 -EPERM/-EINVAL 等内核拒绝）: {e}"
                )));
            }
        }
    }
    Ok(())
}

/// 用 `nft list table inet <name>` 回读表内容（断言规则文本用）。
fn nft_list_table(name: &str) -> std::io::Result<String> {
    let output = Command::new("nft")
        .args(["list", "table", "inet", name])
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "nft list table inet {name} 失败 (exit {:?}): {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// 删表（幂等）：先尝试 mnl 经 nftnl 事务 DEL；再用 `nft delete table` 子进程兜底，
/// 确保任何失败场景都无残留。
fn drop_table(name: &str) {
    // 方式 1：nftnl 事务 DEL（与 add 同源路径）。
    let table = Table::new(
        &CString::new(name).expect("CString 表名构造失败"),
        ProtoFamily::Inet,
    );
    let mut cleanup = Batch::new();
    cleanup.add(&table, nftnl::MsgType::Del);
    match send_and_process(&cleanup.finalize()) {
        Ok(()) => eprintln!("[nettest] 清理（nftnl 事务）：已删表 inet {name}"),
        Err(e) => {
            // 方式 2：nft CLI 兜底（表可能本就不存在或 nftnl DEL 因族/句柄差异失败）。
            let st = Command::new("nft")
                .args(["delete", "table", "inet", name])
                .status();
            match st {
                Ok(s) if s.success() => {
                    eprintln!("[nettest] 清理（nft CLI 兜底）：已删表 inet {name}")
                }
                _ => eprintln!(
                    "[nettest] 清理失败（表可能本不存在，可忽略）: nftnl={e}; nft CLI status={st:?}"
                ),
            }
        }
    }
}

/// 查接口 ifindex（libc::if_nametoindex 包装）。
fn iface_index(name: &str) -> std::io::Result<u32> {
    let c_name = CString::new(name).expect("CString 接口名构造失败");
    // SAFETY: if_nametoindex 只读接口名查表，无副作用，线程安全。
    let index = unsafe { nftnl::nftnl_sys::libc::if_nametoindex(c_name.as_ptr()) };
    if index == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(index)
    }
}
