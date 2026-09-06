//! 防火墙与 NAT
//!
//! 决策依据：规划文档 §3.9 —— 防火墙规则变更必须支持 dry-run + 自动回滚，
//! 避免错误规则锁死管理网。这里定义规则模型与 `Firewall` trait（async）。

use serde::{Deserialize, Serialize};
use std::net::IpAddr;

// ----------------------------------------------------------------------------
// 防火墙规则
// ----------------------------------------------------------------------------

/// 防火墙动作
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FirewallAction {
    /// 放行
    Allow,
    /// 拒绝
    Deny,
    /// 重定向（到本机另一端口，常用于透明代理/转发）
    Redirect,
}

/// 传输层协议
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// TCP
    Tcp,
    /// UDP
    Udp,
    /// 任意（不限协议）
    Any,
}

/// 端口范围/单端口匹配（用字符串以兼容区间表达，如 "80" / "1000-2000"）
pub type PortSpec = String;

/// 校验端口规格字符串。
///
/// 合法形式：
/// - 单端口：`"80"`（1..=65535）；
/// - 区间：`"1000-2000"`（起止均在 1..=65535 且 start ≤ end）。
///
/// 非法返回 `NetworkError::RuleInvalid`。供 `Firewall::dry_run` 复用。
pub fn validate_port_spec(spec: &str) -> Result<(), crate::NetworkError> {
    const PORT_MIN: u32 = 1;
    const PORT_MAX: u32 = 65535;
    if let Some((a, b)) = spec.split_once('-') {
        let start: u32 = a
            .parse()
            .map_err(|_| crate::NetworkError::RuleInvalid(format!("非法端口段起始 `{a}`")))?;
        let end: u32 = b
            .parse()
            .map_err(|_| crate::NetworkError::RuleInvalid(format!("非法端口段结束 `{b}`")))?;
        if !(PORT_MIN..=PORT_MAX).contains(&start) || !(PORT_MIN..=PORT_MAX).contains(&end) {
            return Err(crate::NetworkError::RuleInvalid(format!(
                "端口越界（须 {PORT_MIN}..={PORT_MAX}）: {spec}"
            )));
        }
        if start > end {
            return Err(crate::NetworkError::RuleInvalid(format!(
                "端口段起始大于结束: {spec}"
            )));
        }
    } else {
        let p: u32 = spec
            .parse()
            .map_err(|_| crate::NetworkError::RuleInvalid(format!("非法端口 `{spec}`")))?;
        if !(PORT_MIN..=PORT_MAX).contains(&p) {
            return Err(crate::NetworkError::RuleInvalid(format!(
                "端口越界（须 {PORT_MIN}..={PORT_MAX}）: {spec}"
            )));
        }
    }
    Ok(())
}

/// 防火墙规则
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirewallRule {
    /// 动作
    pub action: FirewallAction,
    /// 协议
    pub protocol: Protocol,
    /// 源地址（None = 任意）
    pub src_addr: Option<IpAddr>,
    /// 源端口（None = 任意）
    pub src_port: Option<PortSpec>,
    /// 目的地址（None = 任意）
    pub dst_addr: Option<IpAddr>,
    /// 目的端口（None = 任意）
    pub dst_port: Option<PortSpec>,
    /// 重定向目标端口（仅 action = Redirect 时有效）
    pub target_port: Option<u16>,
    /// 人类可读说明
    pub description: Option<String>,
}

impl FirewallRule {
    /// 业务合法性自检（不依赖 nft，纯逻辑层）：
    /// - 端口规格（src/dst）须合法；
    /// - `Redirect` 动作必须指定 `target_port`；
    /// - `Redirect` 的 `target_port` 须落在 1..=65535。
    ///
    /// 供 `Firewall::dry_run` 实现复用，也供调用方前置校验。
    pub fn validate(&self) -> Result<(), crate::NetworkError> {
        if let Some(sp) = &self.src_port {
            validate_port_spec(sp)?;
        }
        if let Some(dp) = &self.dst_port {
            validate_port_spec(dp)?;
        }
        if matches!(self.action, FirewallAction::Redirect) {
            let tp = self.target_port.ok_or_else(|| {
                crate::NetworkError::RuleInvalid("Redirect 动作须指定 target_port".into())
            })?;
            if !(1..=65535).contains(&tp) {
                return Err(crate::NetworkError::RuleInvalid(format!(
                    "target_port {tp} 越界（须 1..=65535）"
                )));
            }
        }
        Ok(())
    }
}

// ----------------------------------------------------------------------------
// NAT 规则
// ----------------------------------------------------------------------------

/// NAT（地址转换）规则
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NatRule {
    /// 协议
    pub protocol: Protocol,
    /// 源地址/网段
    pub src: IpAddr,
    /// 转换后地址
    pub translated_addr: IpAddr,
    /// 转换后端口（SNAT/PAT；None = 不改端口）
    pub translated_port: Option<u16>,
}

// ----------------------------------------------------------------------------
// Firewall trait（async）
// ----------------------------------------------------------------------------

/// 防火墙管理器——规则与 NAT 的增删改查，所有变更支持 dry-run + 自动回滚。
///
/// 安全约束（§3.9）：
/// - `add_rule` 默认先 `dry_run` 校验，确认不会切断当前管理连接后才提交；
/// - 提交后启动短期回滚看门狗，超时未确认则自动撤销（防止锁死管理网）。
#[allow(async_fn_in_trait)]
pub trait Firewall: Send + Sync {
    /// 列出当前生效的规则。
    async fn list_rules(&self) -> Result<Vec<FirewallRule>, crate::NetworkError>;

    /// 新增规则；返回生成的规则 ID（用于后续删除/引用）。
    async fn add_rule(&self, rule: FirewallRule) -> Result<String, crate::NetworkError>;

    /// 删除指定规则。
    async fn delete_rule(&self, id: &str) -> Result<(), crate::NetworkError>;

    /// 试运行——在不实际生效的前提下校验规则合法性与影响（如是否会断开管理网）。
    ///
    /// 通过返回 `Ok(())`；有风险返回 `RuleInvalid`。
    async fn dry_run(&self, rule: &FirewallRule) -> Result<(), crate::NetworkError>;

    /// 新增 NAT 规则。
    async fn add_nat(&self, rule: NatRule) -> Result<(), crate::NetworkError>;

    /// 删除 NAT 规则。
    async fn delete_nat(&self, rule: &NatRule) -> Result<(), crate::NetworkError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn port_spec_single_valid() {
        assert!(validate_port_spec("1").is_ok());
        assert!(validate_port_spec("80").is_ok());
        assert!(validate_port_spec("65535").is_ok());
    }

    #[test]
    fn port_spec_range_valid() {
        assert!(validate_port_spec("1000-2000").is_ok());
        assert!(validate_port_spec("80-80").is_ok());
    }

    #[test]
    fn port_spec_rejects_out_of_range() {
        assert!(validate_port_spec("0").is_err());
        assert!(validate_port_spec("65536").is_err());
        assert!(validate_port_spec("1-65536").is_err());
    }

    #[test]
    fn port_spec_rejects_inverted_range() {
        assert!(validate_port_spec("2000-1000").is_err());
    }

    #[test]
    fn port_spec_rejects_non_numeric() {
        assert!(validate_port_spec("abc").is_err());
        assert!(validate_port_spec("80-abc").is_err());
    }

    fn rule(action: FirewallAction, dst_port: Option<&str>, target: Option<u16>) -> FirewallRule {
        FirewallRule {
            action,
            protocol: Protocol::Tcp,
            src_addr: None,
            src_port: None,
            dst_addr: None,
            dst_port: dst_port.map(String::from),
            target_port: target,
            description: None,
        }
    }

    #[test]
    fn firewall_rule_validate_allow_ok() {
        assert!(rule(FirewallAction::Allow, Some("80"), None)
            .validate()
            .is_ok());
    }

    #[test]
    fn firewall_rule_validate_redirect_requires_target() {
        assert!(rule(FirewallAction::Redirect, Some("443"), None)
            .validate()
            .is_err());
        assert!(rule(FirewallAction::Redirect, Some("443"), Some(8443))
            .validate()
            .is_ok());
    }

    #[test]
    fn firewall_rule_validate_redirect_target_range() {
        assert!(rule(FirewallAction::Redirect, Some("443"), Some(0))
            .validate()
            .is_err());
        // target_port 为 u16，编译期已保证上界；测合法上界 65535
        assert!(rule(FirewallAction::Redirect, Some("443"), Some(65535))
            .validate()
            .is_ok());
    }

    #[test]
    fn firewall_rule_validate_bad_port() {
        assert!(rule(FirewallAction::Allow, Some("0"), None)
            .validate()
            .is_err());
        assert!(rule(FirewallAction::Allow, Some("99999"), None)
            .validate()
            .is_err());
    }

    #[test]
    fn nat_rule_construct() {
        let _ = NatRule {
            protocol: Protocol::Tcp,
            src: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            translated_addr: IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1)),
            translated_port: Some(12345),
        };
    }
}
