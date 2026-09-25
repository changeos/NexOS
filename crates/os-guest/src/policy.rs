//! RBAC 策略引擎——条件→Allow/Deny
//!
//! 决策依据：规划文档 §3.18 —— 访客能否执行某动作（入 IM 群/访问共享/用带宽/认证）
//! 由策略规则集判定。规则按 priority 降序匹配，首条匹配规则的决定生效。

use os_core::{DateTime, Deserialize, Serialize, ShareId, Utc};
use os_wallet::VerificationFactor;

use crate::model::{GuestIdentity, GuestIdentityType};

// ----------------------------------------------------------------------------
// PolicyCondition / PolicyEffect / PolicyRule
// ----------------------------------------------------------------------------

/// 策略触发条件
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PolicyCondition {
    /// 恒成立（默认放行/拒绝）
    Always,
    /// 按访客身份类型匹配
    GuestType(GuestIdentityType),
    /// 要求访客已完成指定验证因子（链上签名/余额/凭证，见 os-wallet）
    VerifiedFactor(VerificationFactor),
    /// 时间窗（仅在该时段内允许）
    TimeWindow {
        /// 起始时间（HH:MM，本地时区）
        start: String,
        /// 结束时间（HH:MM，本地时区）
        end: String,
    },
    /// 带宽低于阈值（kbps）
    BandwidthUnder {
        /// 带宽上限（kbps）
        kbps: u64,
    },
}

/// 策略效果
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyEffect {
    /// 允许
    Allow,
    /// 拒绝
    Deny,
}

/// 策略规则（condition 命中时产生 effect）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyRule {
    /// 触发条件
    pub condition: PolicyCondition,
    /// 效果（Allow/Deny）
    pub effect: PolicyEffect,
    /// 优先级（数值越大越优先匹配）
    pub priority: i32,
}

// ----------------------------------------------------------------------------
// GuestAction / GuestContext / PolicyDecision
// ----------------------------------------------------------------------------

/// 访客请求执行的动作
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GuestAction {
    /// 加入 IM 群组
    JoinImGroup {
        /// 目标群组名
        group: String,
    },
    /// 访问文件共享
    AccessShare {
        /// 目标共享 ID
        share: ShareId,
    },
    /// 使用带宽
    UseBandwidth {
        /// 本次请求带宽（kbps）
        kbps: u64,
    },
    /// 发起认证（链上或常规）
    Authenticate,
}

/// 访客决策上下文（请求时刻的运行时信息）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestContext {
    /// 访客来源 IP
    pub ip: String,
    /// 当前时间
    pub current_time: DateTime,
    /// 访客已完成的验证因子（链上签名/余额/凭证）
    pub requested_factors: Vec<VerificationFactor>,
}

impl GuestContext {
    /// 构造一个当前时间戳的上下文
    pub fn new(ip: impl Into<String>) -> Self {
        Self {
            ip: ip.into(),
            current_time: Utc::now(),
            requested_factors: Vec::new(),
        }
    }

    /// 追加一个已完成因子（链式）。
    pub fn with_factor(mut self, factor: VerificationFactor) -> Self {
        self.requested_factors.push(factor);
        self
    }
}

// ----------------------------------------------------------------------------
// 条件匹配 / 规则评估（纯逻辑，可独立单测）
// ----------------------------------------------------------------------------

/// 判断条件是否在给定上下文与访客身份下成立（纯判定，无副作用）。
///
/// 匹配语义：
/// - `Always`：恒成立。
/// - `GuestType(t)`：访客身份类型 == t。
/// - `VerifiedFactor(f)`：上下文 requested_factors 包含等价因子（SignatureChallenge
///   仅判存在；BalanceThreshold 要求 min_amount ≤ 上下文中已声明的任一阈值；
///   Credential 要求 spec 相同）。
/// - `TimeWindow{start,end}`：current_time 的 HH:MM 落在 [start, end] 内。
/// - `BandwidthUnder{kbps}`：仅对 `GuestAction::UseBandwidth` 有意义，判定请求
///   带宽 ≤ kbps；其他动作视为不匹配。
pub fn condition_matches(
    cond: &PolicyCondition,
    guest: &GuestIdentity,
    action: &GuestAction,
    ctx: &GuestContext,
) -> bool {
    match cond {
        PolicyCondition::Always => true,
        PolicyCondition::GuestType(t) => guest.identity_type == *t,
        PolicyCondition::VerifiedFactor(required) => factor_satisfied(required, ctx),
        PolicyCondition::TimeWindow { start, end } => in_time_window(ctx.current_time, start, end),
        PolicyCondition::BandwidthUnder { kbps } => match action {
            GuestAction::UseBandwidth { kbps: req } => req <= kbps,
            _ => false,
        },
    }
}

/// 判定"已完成的因子"是否满足"要求的因子"。
///
/// - `SignatureChallenge`：上下文只要包含任一 `SignatureChallenge` 即满足。
/// - `BalanceThreshold{min_amount}`：上下文任一 `BalanceThreshold` 的阈值
///   ≥ 要求的 min_amount 视为满足（"持币≥阈值"红线：余额通过仅是因子之一，
///   不等价身份可信，需配合其他因子/规则）。
/// - `Credential{spec}`：上下文存在相同 spec 的 Credential 因子即满足。
fn factor_satisfied(required: &VerificationFactor, ctx: &GuestContext) -> bool {
    ctx.requested_factors
        .iter()
        .any(|got| match (required, got) {
            (VerificationFactor::SignatureChallenge, VerificationFactor::SignatureChallenge) => {
                true
            }
            (
                VerificationFactor::BalanceThreshold { min_amount: req },
                VerificationFactor::BalanceThreshold { min_amount: have },
            ) => have >= req,
            (
                VerificationFactor::Credential { spec: req },
                VerificationFactor::Credential { spec: have },
            ) => req == have,
            _ => false,
        })
}

/// 解析 `HH:MM` 为当天自午夜起的分钟数；非法返回 None。
fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    let h: u32 = h.parse().ok()?;
    let m: u32 = m.parse().ok()?;
    if h < 24 && m < 60 {
        Some(h * 60 + m)
    } else {
        None
    }
}

/// 判定 `now` 的本地 HH:MM 是否落在 [`start`, `end`] 闭区间。
///
/// 支持跨午夜区间（如 `22:00`..`06:00`）。非法格式返回 false。
pub fn in_time_window(now: DateTime, start: &str, end: &str) -> bool {
    let (Some(s), Some(e)) = (parse_hhmm(start), parse_hhmm(end)) else {
        return false;
    };
    let cur = now.format("%H:%M").to_string();
    let Some(cur_min) = parse_hhmm(&cur) else {
        return false;
    };
    if s <= e {
        cur_min >= s && cur_min <= e
    } else {
        // 跨午夜：22:00..06:00 → cur >= 22 或 cur <= 06
        cur_min >= s || cur_min <= e
    }
}

/// 规则评估结果（内部用，便于实现复用）。
pub struct EvalOutcome {
    /// 是否允许
    pub allowed: bool,
    /// 命中规则的人类可读标识（None = 无命中，走默认拒绝）
    pub matched: Option<String>,
    /// 决策原因
    pub reason: String,
}

/// 按规则集评估动作，返回决策（纯逻辑，无 IO）。
///
/// 算法（呼应规格书 §3/§5）：
/// 1. 规则按 priority **降序**排序（priority 大者先评估）；
/// 2. 同 priority 下按给定顺序保持稳定（stable sort）；
/// 3. 依次判定每条规则的 condition，**首条命中**即产生 effect 生效；
/// 4. 无任何命中 → 默认拒绝（reason="无匹配规则，默认拒绝"）。
///
/// `rule_id_of`：把 (rule, index) 映射为可读 ID（实现可用 index 或外部 ID 表）。
pub fn evaluate_rules(
    rules: &[PolicyRule],
    guest: &GuestIdentity,
    action: &GuestAction,
    ctx: &GuestContext,
    rule_id_of: impl Fn(&(PolicyRule, usize)) -> String,
) -> EvalOutcome {
    // 配对索引后按 priority 降序稳定排序。
    let mut indexed: Vec<(PolicyRule, usize)> = rules.iter().cloned().zip(0..rules.len()).collect();
    indexed.sort_by_key(|entry| std::cmp::Reverse(entry.0.priority));

    for entry in &indexed {
        if condition_matches(&entry.0.condition, guest, action, ctx) {
            let id = rule_id_of(entry);
            let allowed = matches!(entry.0.effect, PolicyEffect::Allow);
            let reason = format!(
                "命中规则 {} (priority={}, effect={:?})",
                id, entry.0.priority, entry.0.effect
            );
            return EvalOutcome {
                allowed,
                matched: Some(id),
                reason,
            };
        }
    }
    EvalOutcome {
        allowed: false,
        matched: None,
        reason: "无匹配规则，默认拒绝".to_string(),
    }
}

/// 把 `EvalOutcome` 转为 `PolicyDecision`。
impl EvalOutcome {
    pub fn into_decision(self) -> PolicyDecision {
        PolicyDecision {
            allowed: self.allowed,
            matched_rule: self.matched,
            reason: self.reason,
        }
    }
}

/// 策略决策结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyDecision {
    /// 是否允许
    pub allowed: bool,
    /// 命中的规则 ID（None = 无规则匹配，按默认策略）
    pub matched_rule: Option<String>,
    /// 决策原因（人类可读，含命中规则说明）
    pub reason: String,
}

// ----------------------------------------------------------------------------
// PolicyEngine trait（async）
// ----------------------------------------------------------------------------

/// RBAC 策略引擎——按规则集评估访客动作，并管理规则 CRUD。
///
/// 实现者：`DefaultPolicyEngine`（默认，规则存于 os-meta 分布式 KV）；
/// 规则按 priority 降序匹配，首条命中生效，无命中走默认拒绝。
#[allow(async_fn_in_trait)]
pub trait PolicyEngine: Send + Sync {
    /// 评估——判定访客在当前上下文下能否执行动作。
    async fn evaluate(
        &self,
        guest: &GuestIdentity,
        action: &GuestAction,
        context: &GuestContext,
    ) -> Result<PolicyDecision, crate::GuestError>;

    /// 新增规则，返回生成的 rule id。
    async fn add_rule(&self, rule: PolicyRule) -> Result<String, crate::GuestError>;

    /// 按 id 删除规则。
    async fn delete_rule(&self, id: &str) -> Result<(), crate::GuestError>;

    /// 列出全部规则（按 priority 降序）。
    async fn list_rules(&self) -> Vec<PolicyRule>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{GuestIdentityType, GuestStatus};
    use os_core::DateTime;

    fn guest(t: GuestIdentityType) -> GuestIdentity {
        GuestIdentity {
            id: os_core::GuestId::new("GUEST-AAAAAA"),
            identity_type: t,
            created_at: Utc::now(),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            jwt_expiry: Utc::now() + chrono::Duration::minutes(30),
            nft_timeout_secs: 0,
            status: GuestStatus::Authed,
            metadata: serde_json::Value::Null,
        }
    }

    fn ctx_at(h: u32, m: u32) -> GuestContext {
        GuestContext {
            ip: "10.0.0.1".into(),
            current_time: chrono::DateTime::from_timestamp(0, 0).unwrap()
                + chrono::Duration::hours(h as i64)
                + chrono::Duration::minutes(m as i64),
            requested_factors: Vec::new(),
        }
    }

    #[test]
    fn parse_hhmm_ok_and_fail() {
        assert_eq!(parse_hhmm("00:00"), Some(0));
        assert_eq!(parse_hhmm("23:59"), Some(23 * 60 + 59));
        assert_eq!(parse_hhmm("12:30"), Some(750));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("12:60"), None);
        assert_eq!(parse_hhmm("abc"), None);
    }

    #[test]
    fn time_window_normal_and_overnight() {
        let now = ctx_at(10, 30);
        assert!(in_time_window(now.current_time, "08:00", "18:00"));
        assert!(!in_time_window(now.current_time, "18:00", "08:00")); // 跨夜
                                                                      // 跨夜边界
        let night = ctx_at(23, 0);
        assert!(in_time_window(night.current_time, "22:00", "06:00"));
        let morning = ctx_at(5, 0);
        assert!(in_time_window(morning.current_time, "22:00", "06:00"));
        // 非法格式
        assert!(!in_time_window(now.current_time, "xx", "yy"));
    }

    #[test]
    fn factor_satisfied_semantics() {
        let mut ctx = GuestContext::new("10.0.0.1");
        ctx.requested_factors = vec![
            VerificationFactor::SignatureChallenge,
            VerificationFactor::BalanceThreshold { min_amount: 500 },
            VerificationFactor::Credential {
                spec: os_wallet::CredentialSpec::Erc721 {
                    contract: "0xc".into(),
                    token_id: "1".into(),
                },
            },
        ];
        // 签名因子存在即满足。
        assert!(factor_satisfied(
            &VerificationFactor::SignatureChallenge,
            &ctx
        ));
        // 余额阈值：要求 ≤ 已有 → 满足。
        assert!(factor_satisfied(
            &VerificationFactor::BalanceThreshold { min_amount: 500 },
            &ctx
        ));
        assert!(factor_satisfied(
            &VerificationFactor::BalanceThreshold { min_amount: 100 },
            &ctx
        ));
        // 要求 > 已有 → 不满足。
        assert!(!factor_satisfied(
            &VerificationFactor::BalanceThreshold { min_amount: 501 },
            &ctx
        ));
        // 凭证 spec 相同 → 满足。
        assert!(factor_satisfied(
            &VerificationFactor::Credential {
                spec: os_wallet::CredentialSpec::Erc721 {
                    contract: "0xc".into(),
                    token_id: "1".into(),
                },
            },
            &ctx
        ));
        // spec 不同 → 不满足。
        assert!(!factor_satisfied(
            &VerificationFactor::Credential {
                spec: os_wallet::CredentialSpec::Erc721 {
                    contract: "0xd".into(),
                    token_id: "1".into(),
                },
            },
            &ctx
        ));
    }

    #[test]
    fn evaluate_priority_order_first_match_wins() {
        // 高 priority Allow + 低 priority Deny；同条件下高优先 Allow 生效。
        let rules = vec![
            PolicyRule {
                condition: PolicyCondition::Always,
                effect: PolicyEffect::Deny,
                priority: 1,
            },
            PolicyRule {
                condition: PolicyCondition::Always,
                effect: PolicyEffect::Allow,
                priority: 10,
            },
        ];
        let g = guest(GuestIdentityType::RandomId);
        let ctx = GuestContext::new("1.1.1.1");
        let out = evaluate_rules(&rules, &g, &GuestAction::Authenticate, &ctx, |(_, i)| {
            format!("r{i}")
        });
        assert!(out.allowed);
        assert_eq!(out.matched.as_deref(), Some("r1"));
    }

    #[test]
    fn evaluate_no_match_default_deny() {
        let rules = vec![PolicyRule {
            condition: PolicyCondition::GuestType(GuestIdentityType::ChainCredential),
            effect: PolicyEffect::Allow,
            priority: 5,
        }];
        let g = guest(GuestIdentityType::RandomId); // 不匹配
        let ctx = GuestContext::new("1.1.1.1");
        let out = evaluate_rules(&rules, &g, &GuestAction::Authenticate, &ctx, |(_, i)| {
            format!("r{i}")
        });
        assert!(!out.allowed);
        assert!(out.matched.is_none());
        assert!(out.reason.contains("默认拒绝"));
    }

    #[test]
    fn evaluate_bandwidth_condition_only_for_use_bandwidth() {
        let rules = vec![PolicyRule {
            condition: PolicyCondition::BandwidthUnder { kbps: 1000 },
            effect: PolicyEffect::Allow,
            priority: 5,
        }];
        let g = guest(GuestIdentityType::RandomId);
        let ctx = GuestContext::new("1.1.1.1");
        // UseBandwidth 在阈值内 → 命中 Allow。
        let out = evaluate_rules(
            &rules,
            &g,
            &GuestAction::UseBandwidth { kbps: 500 },
            &ctx,
            |(_, i)| format!("r{i}"),
        );
        assert!(out.allowed);
        // 超阈值 → 不命中 → 默认拒绝。
        let out2 = evaluate_rules(
            &rules,
            &g,
            &GuestAction::UseBandwidth { kbps: 2000 },
            &ctx,
            |(_, i)| format!("r{i}"),
        );
        assert!(!out2.allowed);
        // 非 UseBandwidth 动作 → 条件视为不匹配。
        let out3 = evaluate_rules(&rules, &g, &GuestAction::Authenticate, &ctx, |(_, i)| {
            format!("r{i}")
        });
        assert!(!out3.allowed);
    }

    #[test]
    fn evaluate_stable_order_within_same_priority() {
        // 两条同 priority：Deny 在前应先命中（稳定排序保留原序）。
        let rules = vec![
            PolicyRule {
                condition: PolicyCondition::Always,
                effect: PolicyEffect::Deny,
                priority: 5,
            },
            PolicyRule {
                condition: PolicyCondition::Always,
                effect: PolicyEffect::Allow,
                priority: 5,
            },
        ];
        let g = guest(GuestIdentityType::RandomId);
        let ctx = GuestContext::new("1.1.1.1");
        let out = evaluate_rules(&rules, &g, &GuestAction::Authenticate, &ctx, |(_, i)| {
            format!("r{i}")
        });
        assert!(!out.allowed);
        assert_eq!(out.matched.as_deref(), Some("r0"));
    }

    #[test]
    fn into_decision_carries_fields() {
        let out = EvalOutcome {
            allowed: true,
            matched: Some("r0".into()),
            reason: "ok".into(),
        };
        let d: PolicyDecision = out.into_decision();
        assert!(d.allowed);
        assert_eq!(d.matched_rule.as_deref(), Some("r0"));
        assert_eq!(d.reason, "ok");
    }

    // 抑制未使用导入警告（DateTime 在 type alias 中保留以备未来扩展）。
    #[allow(dead_code)]
    fn _dt(_: DateTime) {}
}
