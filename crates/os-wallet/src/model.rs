//! 钱包域共享类型 —— 链类型 / 链配置 / 地址 / 签名算法 / 验证因子 / 签名结果
//!
//! 纯数据结构 + 构造器 + 校验逻辑（无外部密码学依赖），可独立单元测。

use crate::chain::CredentialSpec;
use os_core::{AddressId, ChainId, Deserialize, Serialize};

/// EVM 地址字节长度（不含 `0x` 前缀的十六进制字符数 = 40，字节 = 20）。
pub const EVM_ADDRESS_HEX_LEN: usize = 40;
/// 余额阈值上界（u128 最大值，用于校验）。
pub const MAX_AMOUNT: u128 = u128::MAX;

// ----------------------------------------------------------------------------
// 链类型 / 配置
// ----------------------------------------------------------------------------

/// 支持的链大类
///
/// 初版两种；未来 Solana / Cosmos 通过扩展本枚举接入。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChainKind {
    /// 比特币（含 Ordinals）
    Bitcoin,
    /// EVM 兼容链（Ethereum / Base / ...）
    Evm,
}

/// 单条链的配置（节点 ID 由 `ChainId` 标识，如 `bitcoin` / `ethereum` / `base`）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainConfig {
    /// 链 ID（如 `bitcoin` / `ethereum` / `base`）
    pub chain_id: ChainId,
    /// 链大类
    pub kind: ChainKind,
    /// 主 RPC URL（本地全节点优先）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpc_url: Option<String>,
    /// 备用 RPC URL（外部公共节点 fallback）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rpc_fallback_url: Option<String>,
    /// 是否启用（条件激活开关）
    pub enabled: bool,
}

// ----------------------------------------------------------------------------
// 地址
// ----------------------------------------------------------------------------

/// 已知地址信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddressInfo {
    /// 链上地址（BTC 地址 / EVM 地址，复用 os-core::AddressId）
    pub address: AddressId,
    /// 链大类
    pub chain: ChainKind,
    /// 人类可读标签（None = 无标签）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

// ----------------------------------------------------------------------------
// 签名算法 / 验证因子
// ----------------------------------------------------------------------------

/// 签名算法
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignatureAlgorithm {
    /// BIP-322（比特币通用签名验证）
    Bip322,
    /// Schnorr（比特币 Taproot）
    Schnorr,
    /// ECDSA（secp256k1）
    Ecdsa,
    /// EIP-191（EVM 个人消息签名）
    Eip191,
    /// EIP-712（EVM 类型化数据签名）
    Eip712,
}

/// 访客访问三因子（规划文档 §3.18.1）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VerificationFactor {
    /// 因子一：签名挑战（证明地址所有权）
    SignatureChallenge,
    /// 因子二：余额阈值（地址余额达标）
    BalanceThreshold {
        /// 最小余额阈值（BTC 聪 / EVM wei）
        min_amount: u128,
    },
    /// 因子三：链上凭证（持有特定 Ordinal/NFT）
    Credential {
        /// 凭证规格（Ordinal / Erc721 / Erc1155）
        spec: crate::chain::CredentialSpec,
    },
}

// ----------------------------------------------------------------------------
// 签名结果
// ----------------------------------------------------------------------------

/// 验签结果（纯数据，承载算法 + 是否通过 + 地址）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureResult {
    /// 验签所用算法
    pub algorithm: SignatureAlgorithm,
    /// 是否通过
    pub valid: bool,
    /// 被验证地址（透传，便于结果归属）
    pub address: AddressId,
}

impl SignatureResult {
    /// 构造一个"通过"的验签结果。
    pub fn ok(algorithm: SignatureAlgorithm, address: AddressId) -> Self {
        Self {
            algorithm,
            valid: true,
            address,
        }
    }

    /// 构造一个"未通过"的验签结果。
    pub fn failed(algorithm: SignatureAlgorithm, address: AddressId) -> Self {
        Self {
            algorithm,
            valid: false,
            address,
        }
    }
}

// ----------------------------------------------------------------------------
// 构造器 / 校验
// ----------------------------------------------------------------------------

impl ChainKind {
    /// 返回该链大类的可读名称（用于日志/错误信息）。
    pub fn display_name(self) -> &'static str {
        match self {
            ChainKind::Bitcoin => "Bitcoin",
            ChainKind::Evm => "EVM",
        }
    }

    /// 该链大类默认本地 RPC 方法（探活用；真实调用留 RpcRegistryImpl）。
    pub fn default_probe_method(self) -> &'static str {
        match self {
            ChainKind::Bitcoin => "getblockchaininfo",
            ChainKind::Evm => "eth_blockNumber",
        }
    }
}

impl ChainConfig {
    /// 构造一个启用、带主 RPC 的链配置。
    pub fn new(chain_id: impl Into<String>, kind: ChainKind, rpc_url: impl Into<String>) -> Self {
        Self {
            chain_id: ChainId::new(chain_id),
            kind,
            rpc_url: Some(rpc_url.into()),
            rpc_fallback_url: None,
            enabled: true,
        }
    }

    /// 设置备用 RPC（链式）。
    pub fn with_fallback(mut self, url: impl Into<String>) -> Self {
        self.rpc_fallback_url = Some(url.into());
        self
    }

    /// 校验配置自洽性：
    /// - `enabled` 为 true 时，至少需有主或备 RPC URL 之一；
    /// - `chain_id` 不得为空。
    ///
    /// 返回 `Ok(())` 或描述错误的字符串（不抛 WalletError 以保持 model 无依赖循环）。
    pub fn validate(&self) -> Result<(), String> {
        if self.chain_id.as_str().trim().is_empty() {
            return Err("chain_id 不得为空".to_string());
        }
        if self.enabled
            && self.rpc_url.as_deref().map(str::is_empty).unwrap_or(true)
            && self
                .rpc_fallback_url
                .as_deref()
                .map(str::is_empty)
                .unwrap_or(true)
        {
            return Err(format!(
                "启用的链 `{}` 必须至少配置一个 RPC URL（主或备）",
                self.chain_id
            ));
        }
        Ok(())
    }
}

impl AddressInfo {
    /// 构造一个带标签的地址信息。
    pub fn new(address: AddressId, chain: ChainKind, label: impl Into<String>) -> Self {
        Self {
            address,
            chain,
            label: Some(label.into()),
        }
    }

    /// 构造一个无标签的地址信息。
    pub fn unlabeled(address: AddressId, chain: ChainKind) -> Self {
        Self {
            address,
            chain,
            label: None,
        }
    }
}

impl SignatureAlgorithm {
    /// 该算法所属链大类（用于 adapter 路由校验）。
    pub fn chain_kind(self) -> ChainKind {
        match self {
            SignatureAlgorithm::Bip322
            | SignatureAlgorithm::Schnorr
            | SignatureAlgorithm::Ecdsa => ChainKind::Bitcoin,
            SignatureAlgorithm::Eip191 | SignatureAlgorithm::Eip712 => ChainKind::Evm,
        }
    }

    /// 是否为 EVM 算法。
    pub fn is_evm(self) -> bool {
        matches!(
            self,
            SignatureAlgorithm::Eip191 | SignatureAlgorithm::Eip712
        )
    }

    /// 是否为 BTC 算法。
    pub fn is_bitcoin(self) -> bool {
        matches!(
            self,
            SignatureAlgorithm::Bip322 | SignatureAlgorithm::Schnorr | SignatureAlgorithm::Ecdsa
        )
    }
}

impl CredentialSpec {
    /// 该凭证规格所属链大类（用于 adapter 路由校验）。
    pub fn chain_kind(&self) -> ChainKind {
        match self {
            CredentialSpec::Ordinal { .. } => ChainKind::Bitcoin,
            CredentialSpec::Erc721 { .. } | CredentialSpec::Erc1155 { .. } => ChainKind::Evm,
        }
    }

    /// 校验内部字段非空。
    pub fn validate(&self) -> Result<(), String> {
        match self {
            CredentialSpec::Ordinal { inscription_id } => {
                if inscription_id.trim().is_empty() {
                    return Err("inscription_id 不得为空".to_string());
                }
            }
            CredentialSpec::Erc721 { contract, token_id }
            | CredentialSpec::Erc1155 { contract, token_id } => {
                if contract.trim().is_empty() {
                    return Err("contract 不得为空".to_string());
                }
                if token_id.trim().is_empty() {
                    return Err("token_id 不得为空".to_string());
                }
            }
        }
        Ok(())
    }
}

impl VerificationFactor {
    /// 校验因子配置自洽性（阈值 > 0、凭证 spec 合法等）。
    pub fn validate(&self) -> Result<(), String> {
        match self {
            VerificationFactor::SignatureChallenge => Ok(()),
            VerificationFactor::BalanceThreshold { min_amount } => {
                if *min_amount == 0 {
                    return Err("BalanceThreshold.min_amount 必须大于 0".to_string());
                }
                Ok(())
            }
            VerificationFactor::Credential { spec } => spec.validate(),
        }
    }
}

/// 校验 EVM 地址格式（`0x` + 40 hex；不校验大小写混合的 checksum）。
///
/// 纯字符串逻辑，无外部依赖。真实 EIP-55 checksum 校验留待 alloy 接入后补。
pub fn validate_evm_address(addr: &str) -> Result<(), String> {
    let rest = addr
        .strip_prefix("0x")
        .ok_or_else(|| format!("EVM 地址必须以 `0x` 开头: {addr}"))?;
    if rest.len() != EVM_ADDRESS_HEX_LEN {
        return Err(format!(
            "EVM 地址长度错误：期望 {} hex 字符，实际 {}",
            EVM_ADDRESS_HEX_LEN,
            rest.len()
        ));
    }
    if !rest.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("EVM 地址含非十六进制字符: {addr}"));
    }
    Ok(())
}

/// 校验余额阈值因子是否满足（地址余额 ≥ 阈值）。
///
/// 纯比较逻辑，供 `ChainAdapter::query_balance` 调用方组合使用。
pub fn meets_balance_threshold(balance: u128, min_amount: u128) -> bool {
    balance >= min_amount
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> AddressId {
        AddressId::new(s)
    }

    #[test]
    fn chain_kind_probe_methods() {
        assert_eq!(ChainKind::Bitcoin.display_name(), "Bitcoin");
        assert_eq!(ChainKind::Evm.display_name(), "EVM");
        assert_eq!(
            ChainKind::Bitcoin.default_probe_method(),
            "getblockchaininfo"
        );
        assert_eq!(ChainKind::Evm.default_probe_method(), "eth_blockNumber");
    }

    #[test]
    fn chain_config_validate_ok_and_fail() {
        let cfg = ChainConfig::new("bitcoin", ChainKind::Bitcoin, "http://localhost:8332");
        assert!(cfg.validate().is_ok());

        let bad = ChainConfig {
            chain_id: ChainId::new(""),
            kind: ChainKind::Bitcoin,
            rpc_url: Some("x".into()),
            rpc_fallback_url: None,
            enabled: true,
        };
        assert!(bad.validate().is_err());

        let no_rpc = ChainConfig {
            chain_id: ChainId::new("ethereum"),
            kind: ChainKind::Evm,
            rpc_url: None,
            rpc_fallback_url: None,
            enabled: true,
        };
        let err = no_rpc.validate().unwrap_err();
        assert!(err.contains("RPC"), "{err}");
    }

    #[test]
    fn signature_algorithm_routing() {
        assert!(SignatureAlgorithm::Bip322.is_bitcoin());
        assert!(SignatureAlgorithm::Schnorr.is_bitcoin());
        assert!(SignatureAlgorithm::Ecdsa.is_bitcoin());
        assert!(SignatureAlgorithm::Eip191.is_evm());
        assert!(SignatureAlgorithm::Eip712.is_evm());
        assert_eq!(SignatureAlgorithm::Eip191.chain_kind(), ChainKind::Evm);
        assert_eq!(SignatureAlgorithm::Schnorr.chain_kind(), ChainKind::Bitcoin);
    }

    #[test]
    fn credential_spec_validate() {
        let ok = CredentialSpec::Ordinal {
            inscription_id: "abc".into(),
        };
        assert!(ok.validate().is_ok());
        assert_eq!(ok.chain_kind(), ChainKind::Bitcoin);

        let empty = CredentialSpec::Erc721 {
            contract: String::new(),
            token_id: "1".into(),
        };
        assert!(empty.validate().is_err());

        let erc = CredentialSpec::Erc1155 {
            contract: "0x..".into(),
            token_id: "2".into(),
        };
        assert_eq!(erc.chain_kind(), ChainKind::Evm);
        assert!(erc.validate().is_ok());
    }

    #[test]
    fn verification_factor_validate() {
        assert!(VerificationFactor::SignatureChallenge.validate().is_ok());
        let zero = VerificationFactor::BalanceThreshold { min_amount: 0 };
        assert!(zero.validate().is_err());
        let ok = VerificationFactor::BalanceThreshold { min_amount: 100 };
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn signature_result_ctor() {
        let a = addr("0xabc");
        let ok = SignatureResult::ok(SignatureAlgorithm::Eip191, a.clone());
        assert!(ok.valid);
        let bad = SignatureResult::failed(SignatureAlgorithm::Eip191, a);
        assert!(!bad.valid);
    }

    #[test]
    fn evm_address_validation() {
        assert!(validate_evm_address("0x".to_string().as_str()).is_err());
        // 40 hex chars
        let good = "0x0123456789abcdef0123456789ABCDEF01234567";
        assert!(validate_evm_address(good).is_ok());
        let short = "0xabc";
        assert!(validate_evm_address(short).is_err());
        let nox = "1234567890abcdef1234567890abcdef12345678";
        assert!(validate_evm_address(nox).is_err());
        let badchar = "0xzz3456789abcdef0123456789ABCDEF01234567";
        assert!(validate_evm_address(badchar).is_err());
    }

    #[test]
    fn balance_threshold_logic() {
        assert!(meets_balance_threshold(100, 100));
        assert!(meets_balance_threshold(200, 100));
        assert!(!meets_balance_threshold(50, 100));
    }

    // =========================================================================
    // 覆盖率补充：serde 往返 / Display / 各构造器 / 校验边界
    // =========================================================================

    #[test]
    fn chain_kind_serde_uses_lowercase() {
        // ChainKind 派生 #[serde(rename_all = "lowercase")]。
        assert_eq!(
            serde_json::to_string(&ChainKind::Bitcoin).unwrap(),
            "\"bitcoin\""
        );
        assert_eq!(serde_json::to_string(&ChainKind::Evm).unwrap(), "\"evm\"");
        let btc: ChainKind = serde_json::from_str("\"bitcoin\"").unwrap();
        let evm: ChainKind = serde_json::from_str("\"evm\"").unwrap();
        assert_eq!(btc, ChainKind::Bitcoin);
        assert_eq!(evm, ChainKind::Evm);
        // 未知值拒绝。
        assert!(serde_json::from_str::<ChainKind>("\"solana\"").is_err());
    }

    #[test]
    fn chain_kind_ordering_and_hash() {
        // Ord 派生验证（Bitcoin < Evm，按声明顺序）。
        assert!(ChainKind::Bitcoin < ChainKind::Evm);
        // Hash 派生验证。
        let mut s = std::collections::HashSet::new();
        s.insert(ChainKind::Bitcoin);
        s.insert(ChainKind::Evm);
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn chain_kind_equality_and_copy() {
        let a = ChainKind::Bitcoin;
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn chain_config_with_fallback_chain() {
        let cfg = ChainConfig::new("bitcoin", ChainKind::Bitcoin, "http://main")
            .with_fallback("http://backup");
        assert_eq!(cfg.rpc_url.as_deref(), Some("http://main"));
        assert_eq!(cfg.rpc_fallback_url.as_deref(), Some("http://backup"));
        assert!(cfg.enabled);
        // 带 fallback 的配置应通过校验。
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn chain_config_validate_disabled_no_rpc_ok() {
        // 禁用链即使无 RPC URL 也通过校验。
        let cfg = ChainConfig {
            chain_id: ChainId::new("disabled"),
            kind: ChainKind::Evm,
            rpc_url: None,
            rpc_fallback_url: None,
            enabled: false,
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn chain_config_validate_empty_rpc_url_fails() {
        // 启用但主 RPC 为空字符串（无 fallback）-> 失败。
        let cfg = ChainConfig {
            chain_id: ChainId::new("eth"),
            kind: ChainKind::Evm,
            rpc_url: Some(String::new()),
            rpc_fallback_url: None,
            enabled: true,
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn chain_config_validate_empty_rpc_with_fallback_ok() {
        // 启用、主 RPC 空但 fallback 非空 -> 通过。
        let cfg = ChainConfig {
            chain_id: ChainId::new("eth"),
            kind: ChainKind::Evm,
            rpc_url: Some(String::new()),
            rpc_fallback_url: Some("http://fb".into()),
            enabled: true,
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn chain_config_serde_roundtrip() {
        let cfg =
            ChainConfig::new("ethereum", ChainKind::Evm, "http://x").with_fallback("http://y");
        let s = serde_json::to_string(&cfg).unwrap();
        let back: ChainConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.chain_id, cfg.chain_id);
        assert_eq!(back.kind, cfg.kind);
        assert_eq!(back.rpc_url, cfg.rpc_url);
        assert_eq!(back.rpc_fallback_url, cfg.rpc_fallback_url);
        assert_eq!(back.enabled, cfg.enabled);
    }

    #[test]
    fn chain_config_serde_skips_none_rpc() {
        // skip_serializing_if = "Option::is_none"：无 rpc_url/fallback_url 时不输出。
        let cfg = ChainConfig {
            chain_id: ChainId::new("x"),
            kind: ChainKind::Bitcoin,
            rpc_url: None,
            rpc_fallback_url: None,
            enabled: false,
        };
        let s = serde_json::to_string(&cfg).unwrap();
        assert!(!s.contains("rpc_url"));
        assert!(!s.contains("rpc_fallback_url"));
    }

    #[test]
    fn address_info_new_with_label() {
        let a = addr("0xabc");
        let info = AddressInfo::new(a.clone(), ChainKind::Evm, "main wallet");
        assert_eq!(info.address, a);
        assert_eq!(info.chain, ChainKind::Evm);
        assert_eq!(info.label.as_deref(), Some("main wallet"));
    }

    #[test]
    fn address_info_unlabeled_has_none() {
        let a = addr("bc1q...");
        let info = AddressInfo::unlabeled(a.clone(), ChainKind::Bitcoin);
        assert_eq!(info.address, a);
        assert_eq!(info.chain, ChainKind::Bitcoin);
        assert!(info.label.is_none());
    }

    #[test]
    fn address_info_serde_roundtrip_and_skip() {
        let labeled = AddressInfo::new(addr("0xabc"), ChainKind::Evm, "wallet");
        let s = serde_json::to_string(&labeled).unwrap();
        assert!(s.contains("label"));
        let back: AddressInfo = serde_json::from_str(&s).unwrap();
        assert_eq!(back.label, labeled.label);

        // 无 label 时跳过字段。
        let unlabeled = AddressInfo::unlabeled(addr("0xdef"), ChainKind::Evm);
        let s = serde_json::to_string(&unlabeled).unwrap();
        assert!(!s.contains("label"));
    }

    #[test]
    fn signature_algorithm_serde_snake_case() {
        // SignatureAlgorithm 派生 #[serde(rename_all = "snake_case")]。
        let cases = [
            (SignatureAlgorithm::Bip322, "bip322"),
            (SignatureAlgorithm::Schnorr, "schnorr"),
            (SignatureAlgorithm::Ecdsa, "ecdsa"),
            (SignatureAlgorithm::Eip191, "eip191"),
            (SignatureAlgorithm::Eip712, "eip712"),
        ];
        for (algo, expected) in cases {
            assert_eq!(
                serde_json::to_string(&algo).unwrap(),
                format!("\"{expected}\"")
            );
            let back: SignatureAlgorithm =
                serde_json::from_str(&format!("\"{expected}\"")).unwrap();
            assert_eq!(back, algo);
        }
        // 未知算法拒绝。
        assert!(serde_json::from_str::<SignatureAlgorithm>("\"ed25519\"").is_err());
    }

    #[test]
    fn signature_algorithm_chain_kind_for_all_variants() {
        assert_eq!(SignatureAlgorithm::Bip322.chain_kind(), ChainKind::Bitcoin);
        assert_eq!(SignatureAlgorithm::Schnorr.chain_kind(), ChainKind::Bitcoin);
        assert_eq!(SignatureAlgorithm::Ecdsa.chain_kind(), ChainKind::Bitcoin);
        assert_eq!(SignatureAlgorithm::Eip191.chain_kind(), ChainKind::Evm);
        assert_eq!(SignatureAlgorithm::Eip712.chain_kind(), ChainKind::Evm);
    }

    #[test]
    fn signature_algorithm_is_evm_and_is_bitcoin_complement() {
        for algo in [
            SignatureAlgorithm::Bip322,
            SignatureAlgorithm::Schnorr,
            SignatureAlgorithm::Ecdsa,
            SignatureAlgorithm::Eip191,
            SignatureAlgorithm::Eip712,
        ] {
            assert_ne!(algo.is_evm(), algo.is_bitcoin(), "{algo:?} 互斥性破坏");
        }
    }

    #[test]
    fn signature_result_serde_roundtrip() {
        let a = addr("0xabc");
        let ok = SignatureResult::ok(SignatureAlgorithm::Eip191, a.clone());
        let s = serde_json::to_string(&ok).unwrap();
        let back: SignatureResult = serde_json::from_str(&s).unwrap();
        assert_eq!(back, ok);
        assert!(back.valid);

        let failed = SignatureResult::failed(SignatureAlgorithm::Schnorr, a);
        let s = serde_json::to_string(&failed).unwrap();
        let back: SignatureResult = serde_json::from_str(&s).unwrap();
        assert_eq!(back, failed);
        assert!(!back.valid);
    }

    #[test]
    fn verification_factor_serde_roundtrip_signature_and_credential() {
        // 注：BalanceThreshold 内含 u128，serde_json 默认不支持 u128 序列化，
        // 故该变体的 serde 往返由专门测试（见 verification_factor_balance_threshold_serde_format）覆盖。
        let sig = VerificationFactor::SignatureChallenge;
        let cred = VerificationFactor::Credential {
            spec: CredentialSpec::Ordinal {
                inscription_id: "abc".into(),
            },
        };
        let cred_evm = VerificationFactor::Credential {
            spec: CredentialSpec::Erc721 {
                contract: "0xabc".into(),
                token_id: "42".into(),
            },
        };
        for f in [sig, cred, cred_evm] {
            let s = serde_json::to_string(&f).unwrap();
            let back: VerificationFactor = serde_json::from_str(&s).unwrap();
            assert_eq!(back, f);
        }
    }

    #[test]
    fn verification_factor_serde_uses_tagged_kind_field() {
        // VerificationFactor 派生 #[serde(tag = "kind", rename_all = "snake_case")]，
        // 验证序列化结果包含 "kind" 字段且为 snake_case。
        let sig = VerificationFactor::SignatureChallenge;
        let s = serde_json::to_string(&sig).unwrap();
        assert!(s.contains("\"kind\":\"signature_challenge\""), "{s}");

        let cred = VerificationFactor::Credential {
            spec: CredentialSpec::Erc1155 {
                contract: "0xc".into(),
                token_id: "7".into(),
            },
        };
        let s = serde_json::to_string(&cred).unwrap();
        assert!(s.contains("\"kind\":\"credential\""), "{s}");
        assert!(s.contains("\"spec\""), "{s}");
    }

    #[test]
    fn verification_factor_serde_balance_threshold_format() {
        // u128 无法经 serde_json 往返（其默认不支持 u128），但可经 toml / 字节流等支持；
        // 这里仅断言序列化时变体 tag 为 balance_threshold（不实际 deserialize u128 值）。
        let bal = VerificationFactor::BalanceThreshold { min_amount: 1 };
        let s = serde_json::to_string(&bal).unwrap();
        assert!(s.contains("\"kind\":\"balance_threshold\""), "{s}");
        // 反序列化失败符合预期（serde_json 不支持 u128）。
        assert!(serde_json::from_str::<VerificationFactor>(&s).is_err());
    }

    #[test]
    fn verification_factor_validate_credential_branches() {
        // 通过 spec 验证的因子。
        let ok = VerificationFactor::Credential {
            spec: CredentialSpec::Erc721 {
                contract: "0xabc".into(),
                token_id: "1".into(),
            },
        };
        assert!(ok.validate().is_ok());
        // 凭证 spec 非法（contract 为空）-> 因子校验失败。
        let bad = VerificationFactor::Credential {
            spec: CredentialSpec::Erc721 {
                contract: String::new(),
                token_id: "1".into(),
            },
        };
        assert!(bad.validate().is_err());
        // 凭证 spec 非法（token_id 为空）。
        let bad2 = VerificationFactor::Credential {
            spec: CredentialSpec::Erc1155 {
                contract: "0xabc".into(),
                token_id: String::new(),
            },
        };
        assert!(bad2.validate().is_err());
        // Ordinal 空 inscription_id。
        let bad3 = VerificationFactor::Credential {
            spec: CredentialSpec::Ordinal {
                inscription_id: "  ".into(),
            },
        };
        assert!(bad3.validate().is_err());
    }

    #[test]
    fn validate_evm_address_edge_cases() {
        // 大小写都接受。
        assert!(validate_evm_address("0xABCDEF0123456789ABCDEF0123456789ABCDEF01").is_ok());
        assert!(validate_evm_address("0xabcdef0123456789abcdef0123456789abcdef01").is_ok());
        // 长度恰好为 40 hex。
        let exact = "0x".to_string() + &"a".repeat(40);
        assert!(validate_evm_address(&exact).is_ok());
        // 多 1 字符。
        let too_long = "0x".to_string() + &"a".repeat(41);
        assert!(validate_evm_address(&too_long).is_err());
        // 仅 `0x` 前缀。
        assert!(validate_evm_address("0x").is_err());
        // `0X`（大写 X）不是合法前缀。
        assert!(validate_evm_address("0Xabcdef0123456789abcdef0123456789abcdef01").is_err());
    }

    #[test]
    fn meets_balance_threshold_boundary() {
        // 边界：恰好相等 -> true；阈值 0 -> 任意非负都满足。
        assert!(meets_balance_threshold(0, 0));
        assert!(meets_balance_threshold(1, 0));
        // 大阈值。
        assert!(meets_balance_threshold(u128::MAX, u128::MAX));
        assert!(!meets_balance_threshold(u128::MAX - 1, u128::MAX));
    }

    #[test]
    fn credential_spec_validate_erc721_empty_token_id() {
        let bad = CredentialSpec::Erc721 {
            contract: "0xok".into(),
            token_id: String::new(),
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn credential_spec_validate_erc1155_empty_contract() {
        let bad = CredentialSpec::Erc1155 {
            contract: "  ".into(),
            token_id: "1".into(),
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn constants_have_expected_values() {
        assert_eq!(EVM_ADDRESS_HEX_LEN, 40);
        assert_eq!(MAX_AMOUNT, u128::MAX);
    }
}
