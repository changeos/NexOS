//! nftables guest 链编排——与 os-network 协同
//!
//! 决策依据：规划文档 §3.18 / §4 风险表 —— 访客认证成功后向 nftables guest set
//! 加入放行规则（带 timeout 自动过期）；变更前必须 dry-run，命中冲突则中止；
//! 应用时建 checkpoint，5 分钟内可回滚（高危操作风险表要求）。

use os_core::{Deserialize, Serialize};

// ----------------------------------------------------------------------------
// NftGuestRule / NftGuestAction / DryRunResult
// ----------------------------------------------------------------------------

/// nft guest 动作
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NftGuestAction {
    /// 认证放行（加入 guest set，允许指定端口）
    Authenticate {
        /// 允许访问的端口列表（如 [445, 443]）
        allowed_ports: Vec<u16>,
    },
    /// 取消认证（从 guest set 移除）
    Deauthenticate,
}

/// nft guest 规则
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NftGuestRule {
    /// 访客 IP
    pub guest_ip: String,
    /// 动作
    pub action: NftGuestAction,
    /// 超时（秒；到期自动从 set 过期，0 = 永久）
    pub timeout_secs: u64,
}

/// dry-run 结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DryRunResult {
    /// 实际会变更的 nft 对象列表（如 ["add element guest_set { 10.0.0.5 timeout 3600s }"]）
    pub would_change: Vec<String>,
    /// 命中冲突的列表（与既有规则冲突，非空则应中止应用）
    pub conflicts: Vec<String>,
}

// ----------------------------------------------------------------------------
// nft 规则字符串构造（纯字符串拼接，不调用 nft / nftnl）
// ----------------------------------------------------------------------------

/// guest set 名称（与 os-network 协同：set/chain 由 network 层初始化）。
pub const NFT_GUEST_SET: &str = "guest_set";
/// guest 链名称。
pub const NFT_GUEST_CHAIN: &str = "guest_chain";
/// guest table 名称（默认 inet 全局表 `os`）。
pub const NFT_GUEST_TABLE: &str = "os";

/// 校验 IP 地址合法性（IPv4/IPv6 字符串）。
///
/// 纯字符串逻辑（不依赖 std::net 解析外的依赖）；非法返回 false。
pub fn is_valid_ip(ip: &str) -> bool {
    ip.parse::<std::net::IpAddr>().is_ok()
}

/// 构造 `add element` 语句（加入 guest set 放行）。
///
/// 形如：`add element inet os guest_set { 10.0.0.5 timeout 3600s }`
/// `timeout_secs == 0` 时不附加 timeout（永久）。
pub fn build_add_element(rule: &NftGuestRule) -> Result<String, crate::GuestError> {
    if !is_valid_ip(&rule.guest_ip) {
        return Err(crate::GuestError::NftRuleFailed(format!(
            "非法访客 IP: {}",
            rule.guest_ip
        )));
    }
    let timeout_clause = if rule.timeout_secs > 0 {
        format!(" timeout {}s", rule.timeout_secs)
    } else {
        String::new()
    };
    Ok(format!(
        "add element inet {} {} {{ {}{} }}",
        NFT_GUEST_TABLE, NFT_GUEST_SET, rule.guest_ip, timeout_clause
    ))
}

/// 构造 `delete element` 语句（从 guest set 移除，用于 revoke / Deauthenticate）。
///
/// 形如：`delete element inet os guest_set { 10.0.0.5 }`
pub fn build_delete_element(guest_ip: &str) -> Result<String, crate::GuestError> {
    if !is_valid_ip(guest_ip) {
        return Err(crate::GuestError::NftRuleFailed(format!(
            "非法访客 IP: {guest_ip}"
        )));
    }
    Ok(format!(
        "delete element inet {} {} {{ {} }}",
        NFT_GUEST_TABLE, NFT_GUEST_SET, guest_ip
    ))
}

/// 构造端口放行规则（限定访客可访问的端口）。
///
/// 形如：`add rule inet os guest_chain ip saddr 10.0.0.5 tcp dport { 445, 443 } accept`
/// `allowed_ports` 为空时返回 None（无端口规则需添加）。
pub fn build_port_accept_rule(
    guest_ip: &str,
    allowed_ports: &[u16],
) -> Result<Option<String>, crate::GuestError> {
    if !is_valid_ip(guest_ip) {
        return Err(crate::GuestError::NftRuleFailed(format!(
            "非法访客 IP: {guest_ip}"
        )));
    }
    if allowed_ports.is_empty() {
        return Ok(None);
    }
    let ports: Vec<String> = allowed_ports.iter().map(|p| p.to_string()).collect();
    Ok(Some(format!(
        "add rule inet {} {} ip saddr {} tcp dport {{ {} }} accept",
        NFT_GUEST_TABLE,
        NFT_GUEST_CHAIN,
        guest_ip,
        ports.join(", ")
    )))
}

/// 构造 checkpoint 语句（nft 事务快照，用于回滚）。
///
/// 形如：`add checkpoint`（实际 nft 用 `nft --check` / 事务句柄；此处为可读标识）。
pub fn build_checkpoint_statement(label: &str) -> String {
    format!("checkpoint:{label}")
}

/// 为单条 guest 规则生成完整变更语句列表（add element + 端口 accept 规则）。
///
/// 供 `dry_run` / `apply` 复用：先调本函数得到将要执行的语句，dry_run 时只
/// 构造不执行，apply 时落库到 nft。
pub fn statements_for_rule(rule: &NftGuestRule) -> Result<Vec<String>, crate::GuestError> {
    let mut stmts = vec![build_add_element(rule)?];
    if let NftGuestAction::Authenticate { allowed_ports } = &rule.action {
        if !allowed_ports.is_empty() {
            if let Some(port_rule) = build_port_accept_rule(&rule.guest_ip, allowed_ports)? {
                stmts.push(port_rule);
            }
        }
    }
    Ok(stmts)
}

// ----------------------------------------------------------------------------
// NftRuleOrchestrator trait（async）
// ----------------------------------------------------------------------------

/// nft guest 规则编排器——管理访客放行规则的增删与回滚。
///
/// 实现者：`NftRuleOrchestratorImpl`（默认，调用 nft 命令）；
/// 与 os-network 协同（nft set/chain 由 network 层初始化，本 trait 只管 guest 元素）。
/// 风险控制（§4）：所有变更先 dry-run，应用建 checkpoint，支持回滚。
#[allow(async_fn_in_trait)]
pub trait NftRuleOrchestrator: Send + Sync {
    /// 应用规则（加 nft set，带 timeout 自动过期）。
    /// 实现应先 dry-run，命中 conflicts 则返回 `NftRuleFailed`。
    async fn apply(&self, rule: NftGuestRule) -> Result<(), crate::GuestError>;

    /// 撤销访客（按 IP 移除其全部 guest 规则）。
    async fn revoke(&self, guest_ip: &str) -> Result<(), crate::GuestError>;

    /// 预演——不实际变更，返回将会变更的对象与冲突列表（§4 风险表要求 dry-run）。
    async fn dry_run(&self, rule: &NftGuestRule) -> Result<DryRunResult, crate::GuestError>;

    /// 按 checkpoint 回滚（应用时生成的 checkpoint_id，5 分钟有效期内可回滚）。
    async fn rollback_checkpoint(&self, checkpoint_id: &str) -> Result<(), crate::GuestError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth_rule(ip: &str, ports: &[u16], timeout: u64) -> NftGuestRule {
        NftGuestRule {
            guest_ip: ip.to_string(),
            action: NftGuestAction::Authenticate {
                allowed_ports: ports.to_vec(),
            },
            timeout_secs: timeout,
        }
    }

    #[test]
    fn ip_validation() {
        assert!(is_valid_ip("10.0.0.5"));
        assert!(is_valid_ip("::1"));
        assert!(is_valid_ip("fd00::1"));
        assert!(!is_valid_ip("999.0.0.1"));
        assert!(!is_valid_ip("not-an-ip"));
        assert!(!is_valid_ip(""));
    }

    #[test]
    fn add_element_with_and_without_timeout() {
        let r = auth_rule("10.0.0.5", &[445], 3600);
        let s = build_add_element(&r).unwrap();
        assert!(s.contains("add element inet os guest_set"));
        assert!(s.contains("10.0.0.5"));
        assert!(s.contains("timeout 3600s"));

        let r2 = auth_rule("10.0.0.5", &[], 0);
        let s2 = build_add_element(&r2).unwrap();
        assert!(!s2.contains("timeout"));
    }

    #[test]
    fn add_element_rejects_bad_ip() {
        let r = auth_rule("bad", &[], 0);
        assert!(build_add_element(&r).is_err());
    }

    #[test]
    fn delete_element_format() {
        let s = build_delete_element("10.0.0.5").unwrap();
        assert_eq!(s, "delete element inet os guest_set { 10.0.0.5 }");
        assert!(build_delete_element("bad").is_err());
    }

    #[test]
    fn port_accept_rule_optional() {
        // 有端口 → Some。
        let s = build_port_accept_rule("10.0.0.5", &[445, 443]).unwrap();
        let s = s.unwrap();
        assert!(s.contains("add rule inet os guest_chain"));
        assert!(s.contains("ip saddr 10.0.0.5"));
        assert!(s.contains("tcp dport { 445, 443 } accept"));
        // 无端口 → None。
        assert!(build_port_accept_rule("10.0.0.5", &[]).unwrap().is_none());
        // 非法 IP → Err。
        assert!(build_port_accept_rule("bad", &[80]).is_err());
    }

    #[test]
    fn statements_for_rule_authenticate() {
        let r = auth_rule("10.0.0.5", &[445, 443], 3600);
        let stmts = statements_for_rule(&r).unwrap();
        assert_eq!(stmts.len(), 2);
        assert!(stmts[0].contains("add element"));
        assert!(stmts[1].contains("tcp dport"));
    }

    #[test]
    fn statements_for_rule_deauthenticate_only_element() {
        let r = NftGuestRule {
            guest_ip: "10.0.0.5".to_string(),
            action: NftGuestAction::Deauthenticate,
            timeout_secs: 0,
        };
        let stmts = statements_for_rule(&r).unwrap();
        assert_eq!(stmts.len(), 1);
        assert!(stmts[0].contains("add element"));
    }

    #[test]
    fn checkpoint_statement_format() {
        assert_eq!(
            build_checkpoint_statement("apply-123"),
            "checkpoint:apply-123"
        );
    }
}
