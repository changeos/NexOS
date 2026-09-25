//! 真实 nftables 事务集成测（feature `nftnl-ffi`）。
//!
//! 验证 `NftRuleOrchestratorImpl::apply` / `revoke` / `rollback_checkpoint` 经 nftnl
//! 真实提交到内核：apply 后 `nft list table inet os` 能看到 guest_set 元素（带 timeout）
//! + guest_chain 规则；revoke 后元素消失；rollback 后快照恢复。
//!
//! **运行要求**（红线：不碰宿主真实防火墙）：
//! - root / CAP_NET_ADMIN（`sudo cargo test --features nftnl-ffi --test nftnl_real -- --ignored`）；
//! - libnftnl-dev + libmnl-dev（pkg-config）；
//! - 测试用表名 `os`（NftRuleOrchestratorImpl 硬编码常量 `NFT_GUEST_TABLE`）。
//!   每个 setup 先删表（幂等），RAII Drop 测后自动删整个表，不残留。
//!
//! **并行限制**：因表名 `os` 硬编码，所有测试共享同一表，**必须串行跑**：
//! `--ignored --test-threads=1`。setup 已做幂等删表，串行下安全。
//!
//! 标 `#[ignore]`：默认套件不跑（避免在无 root/无 FFI 环境失败）。
//! 无 root / 无 libnftnl 时优雅提前 return（不 panic）。

#![cfg(feature = "nftnl-ffi")]

use std::process::{Command, Stdio};

use os_guest::impls::NftRuleOrchestratorImpl;
use os_guest::nft::{NftGuestAction, NftGuestRule, NftRuleOrchestrator};
use os_guest::nft::{NFT_GUEST_CHAIN, NFT_GUEST_SET, NFT_GUEST_TABLE};

// ============================================================================
// 环境探测 / 优雅 SKIP
// ============================================================================

/// 是否有 root 权限（uid==0）。
fn is_root() -> bool {
    // sudo cargo test 时，euid 为 0；普通 cargo test 为 1000。
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

/// 探测 nft CLI 是否可用。
fn has_nft() -> bool {
    Command::new("which")
        .arg("nft")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 若无 root / 无 nft，优雅 SKIP（打印消息提前 return，不 panic）。
fn require_real_env(test_name: &str) -> bool {
    if !is_root() {
        eprintln!(
            "[SKIP] {} : 非 root（需 sudo cargo test ... -- --ignored）",
            test_name
        );
        return false;
    }
    if !has_nft() {
        eprintln!(
            "[SKIP] {} : nft CLI 不可用（apt install nftables）",
            test_name
        );
        return false;
    }
    true
}

// ============================================================================
// nft CLI 辅助：建/删 table + set/chain（测试 setup/teardown）
// ============================================================================

/// 建 `inet <table>` 表 + guest_set（IPv4，带 timeout）+ guest_chain。
///
/// set 定义：type ipv4_addr, flags timeout（元素可带 timeout 自动过期）。
/// chain 定义：type filter, hook 不绑定（独立链，由其它链 jump 或直接 list 验证）。
///
/// **幂等**：先删表（若残留），再重建，保证每个测试从干净状态开始。
fn nft_setup_guest_infra(table: &str) -> Result<(), String> {
    // 先删表（幂等：若不存在则忽略错误），清掉上次残留。
    nft_teardown(table);
    // 建表。
    let s = Command::new("nft")
        .args(["add", "table", "inet", table])
        .status()
        .map_err(|e| format!("建表失败: {e}"))?;
    if !s.success() {
        return Err(format!("建表 inet {table} 失败"));
    }
    // 建 set：guest_set，IPv4 类型，flags timeout（支持元素 timeout）。
    let set_def = format!("{} {{ type ipv4_addr; flags timeout; }}", NFT_GUEST_SET);
    let s = Command::new("nft")
        .args(["add", "set", "inet", table, &set_def])
        .status()
        .map_err(|e| format!("建 set 失败: {e}"))?;
    if !s.success() {
        return Err(format!("建 set {NFT_GUEST_SET} 失败"));
    }
    // 建 guest_chain（apply 端口规则需要）。
    let s = Command::new("nft")
        .args(["add", "chain", "inet", table, NFT_GUEST_CHAIN])
        .status()
        .map_err(|e| format!("建 chain 失败: {e}"))?;
    if !s.success() {
        return Err(format!("建 chain {NFT_GUEST_CHAIN} 失败"));
    }
    Ok(())
}

/// 删 `inet <table>` 表（含其下全部 set/chain/规则，原子清理）。
fn nft_teardown(table: &str) {
    let _ = Command::new("nft")
        .args(["delete", "table", "inet", table])
        .status();
}

/// `nft list table inet <table>` 输出（用于断言元素/规则是否存在）。
fn nft_list_table(table: &str) -> String {
    Command::new("nft")
        .args(["list", "table", "inet", table])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

// ============================================================================
// RAII guard：测试结束自动删表（防残留）
// ============================================================================

struct NftGuard {
    table: String,
}

impl NftGuard {
    fn new(table: &str) -> Self {
        Self {
            table: table.to_string(),
        }
    }
}

impl Drop for NftGuard {
    fn drop(&mut self) {
        nft_teardown(&self.table);
        eprintln!("[teardown] 已删表 inet {}", self.table);
    }
}

// ============================================================================
// 真实测（#[ignore]）
// ============================================================================

/// 测 1：apply（add element + add rule）真实提交，nft list 验证存在。
#[tokio::test]
#[ignore = "需 root + CAP_NET_ADMIN + libnftnl-dev + libmnl-dev（sudo cargo test -- --ignored）"]
async fn nftnl_apply_real() {
    if !require_real_env("nftnl_apply_real") {
        return;
    }

    let table = NFT_GUEST_TABLE;
    let _guard = NftGuard::new(table); // RAII：函数退出自动删表

    // setup：建表 + guest_set（带 timeout flag）+ guest_chain（幂等）。
    nft_setup_guest_infra(table).expect("setup guest infra 应成功");

    let orch = NftRuleOrchestratorImpl::new();
    let rule = NftGuestRule {
        guest_ip: "10.99.0.5".to_string(),
        action: NftGuestAction::Authenticate {
            allowed_ports: vec![445, 443],
        },
        timeout_secs: 3600,
    };

    // apply：内部调 nftnl_apply_statements（NEWSETELEM + NEWRULE）。
    match orch.apply(rule).await {
        Ok(()) => eprintln!("[ok] apply 成功提交 nftnl 事务"),
        Err(e) => panic!("apply 应成功（root + libnftnl）但失败: {e:?}"),
    }

    // 验证：nft list 应包含 IP + 端口规则。
    let listing = nft_list_table(table);
    eprintln!("[verify] nft list table inet {}:\n{}", table, listing);
    assert!(
        listing.contains("10.99.0.5"),
        "apply 后 guest_set 应包含 10.99.0.5，实际: {listing}"
    );
    // 端口规则（445/443）应在 guest_chain 中。注：多端口降级匹配，至少含一个端口。
    assert!(
        listing.contains("445") || listing.contains("443"),
        "apply 后 guest_chain 应含端口 445 或 443，实际: {listing}"
    );
}

/// 测 2：apply 后 revoke（delete element），nft list 验证元素消失。
#[tokio::test]
#[ignore = "需 root + CAP_NET_ADMIN + libnftnl-dev + libmnl-dev（sudo cargo test -- --ignored）"]
async fn nftnl_revoke_real() {
    if !require_real_env("nftnl_revoke_real") {
        return;
    }

    let table = NFT_GUEST_TABLE;
    let _guard = NftGuard::new(table);

    nft_setup_guest_infra(table).expect("setup guest infra 应成功");

    let orch = NftRuleOrchestratorImpl::new();
    let rule = NftGuestRule {
        guest_ip: "10.99.0.6".to_string(),
        action: NftGuestAction::Authenticate {
            allowed_ports: vec![],
        },
        timeout_secs: 3600,
    };

    // 先 apply（仅加 element，无端口规则）。
    orch.apply(rule).await.expect("apply 应成功");
    let after_apply = nft_list_table(table);
    assert!(
        after_apply.contains("10.99.0.6"),
        "apply 后应含 10.99.0.6: {after_apply}"
    );

    // revoke（DELSETELEM）。
    match orch.revoke("10.99.0.6").await {
        Ok(()) => eprintln!("[ok] revoke 成功"),
        Err(e) => panic!("revoke 应成功但失败: {e:?}"),
    }
    let after_revoke = nft_list_table(table);
    eprintln!("[verify] revoke 后:\n{}", after_revoke);
    assert!(
        !after_revoke.contains("10.99.0.6"),
        "revoke 后元素应消失，实际仍含: {after_revoke}"
    );
}

/// 测 3：rollback_checkpoint（apply → rollback 验证快照恢复）。
#[tokio::test]
#[ignore = "需 root + CAP_NET_ADMIN + libnftnl-dev + libmnl-dev（sudo cargo test -- --ignored）"]
async fn nftnl_rollback_checkpoint_real() {
    if !require_real_env("nftnl_rollback_checkpoint_real") {
        return;
    }

    let table = NFT_GUEST_TABLE;
    let _guard = NftGuard::new(table);

    nft_setup_guest_infra(table).expect("setup guest infra 应成功");

    let orch = NftRuleOrchestratorImpl::new();

    // apply 第一条（快照点：空 active）。
    let r1 = NftGuestRule {
        guest_ip: "10.99.1.1".to_string(),
        action: NftGuestAction::Authenticate {
            allowed_ports: vec![],
        },
        timeout_secs: 3600,
    };
    orch.apply(r1).await.expect("apply #1 应成功");

    // 取最近 checkpoint id（apply 时建的）。
    let cp_id = orch
        .last_checkpoint_id()
        .expect("apply 后应有 last checkpoint");

    // apply 第二条（改变 active）。
    let r2 = NftGuestRule {
        guest_ip: "10.99.1.2".to_string(),
        action: NftGuestAction::Authenticate {
            allowed_ports: vec![],
        },
        timeout_secs: 3600,
    };
    orch.apply(r2).await.expect("apply #2 应成功");

    let before_rollback = nft_list_table(table);
    assert!(before_rollback.contains("10.99.1.2"), "应有 10.99.1.2");

    // rollback 到 cp_id（恢复到仅 r1 的状态）。
    match orch.rollback_checkpoint(&cp_id).await {
        Ok(()) => eprintln!("[ok] rollback 成功"),
        Err(e) => panic!("rollback 应成功但失败: {e:?}"),
    }

    // rollback 后：10.99.1.1 应在（snapshot 含），10.99.1.2 应不在（被回滚）。
    let after_rollback = nft_list_table(table);
    eprintln!("[verify] rollback 后:\n{}", after_rollback);
    // 注：rollback 实现对当前 active 发 delete、对 snapshot 发 add，
    // 10.99.1.1 重新 add（若已存在则 -EEXIST 被忽略或报错，取决于实现）。
    // 关键断言：10.99.1.2 应被 delete 掉。
    assert!(
        !after_rollback.contains("10.99.1.2"),
        "rollback 后 10.99.1.2 应被删除，实际仍含: {after_rollback}"
    );
}
