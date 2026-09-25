//! 链适配 trait（§3.17 核心抽象）+ 真实 BTC/EVM 实现
//!
//! 实现说明：
//! - `BitcoinAdapter`：基于 rust-bitcoin（Schnorr BIP-340 / ECDSA signmessage），
//!   经 [`crate::signing`] 路由到真实验签。BIP-322 完整伪交易封装留 TODO。
//! - `EvmAdapter`：基于 alloy（EIP-191 / EIP-712），经 [`crate::signing`] 路由到
//!   真实 keccak256 + secp256k1 地址恢复比对。
//! - 余额/凭证查询经可注入的 [`crate::registry::RpcProbe`]（同 RpcRegistryImpl 探活）
//!   发 JSON-RPC：EVM `eth_getBalance` / ERC-20 `eth_call balanceOf`；BTC `scantxoutset`
//!   + Ordinals ord index。无 probe（None）时返回 `Err(Internal)`（优雅降级）。
//!
//! 数据源（§9.1#12）：
//! - EVM：直查本地/远程 RPC
//! - Ordinals：自托管 ord index 优先，外部 fallback 补充

use async_trait::async_trait;
use os_core::{AddressId, Deserialize, Serialize};

use crate::model::{ChainKind, SignatureAlgorithm};
use crate::registry::RpcProbe;
use crate::WalletResult;

// ----------------------------------------------------------------------------
// 凭证规格
// ----------------------------------------------------------------------------

/// 链上凭证规格（持有性查询目标）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CredentialSpec {
    /// 比特币 Ordinal 铭文
    Ordinal {
        /// 铭文 ID
        inscription_id: String,
    },
    /// ERC-721 NFT（单一所有权）
    Erc721 {
        /// 合约地址
        contract: String,
        /// token id
        token_id: String,
    },
    /// ERC-1155 多代币（可批量）
    Erc1155 {
        /// 合约地址
        contract: String,
        /// token id
        token_id: String,
    },
}

// ----------------------------------------------------------------------------
// ChainAdapter trait（async，链无关核心抽象）
// ----------------------------------------------------------------------------

/// 链适配器——链无关的签名验证 / 余额查询 / 凭证查询。
///
/// 各链实现本 trait；`RpcRegistry` 在对应链 RPC 可用时才注册 adapter。
///
/// 经 `Box<dyn ChainAdapter>` 注册到 RpcRegistry，故用 `#[async_trait]` 保证 dyn 兼容（ADR-COMPAT-001）。
#[async_trait]
pub trait ChainAdapter: Send + Sync {
    /// 验证签名（地址所有权证明）。
    async fn verify_signature(
        &self,
        address: &AddressId,
        message: &str,
        signature: &[u8],
        algo: SignatureAlgorithm,
    ) -> WalletResult<bool>;

    /// 查询地址余额（BTC 单位为聪 / EVM 单位为 wei）。
    async fn query_balance(&self, address: &AddressId) -> WalletResult<u128>;

    /// 查询地址是否持有指定凭证（Ordinal/NFT）。
    async fn query_credential(
        &self,
        address: &AddressId,
        cred: CredentialSpec,
    ) -> WalletResult<bool>;

    /// 返回该适配器服务的链大类。
    async fn chain_kind(&self) -> ChainKind;
}

// ============================================================================
// ChainAdapter 真实实现（BTC / EVM）
// ============================================================================

/// 比特币链适配器（BIP-322 / Schnorr / ECDSA）。
///
/// 真实验签基于 rust-bitcoin + secp256k1（见 [`crate::signing`]）：
/// - [`SignatureAlgorithm::Schnorr`]：BIP-340 Schnorr 验签（`address` 字段传
///   x-only 公钥 hex；签名 64 字节）。
/// - [`SignatureAlgorithm::Ecdsa`]：Bitcoin Core signmessage（`address` 字段传
///   公钥 hex 33/65 字节压缩/非压缩）。
/// - [`SignatureAlgorithm::Bip322`]：完整 BIP-322 伪交易封装（留 TODO，业务侧
///   应优先用 Schnorr/ECDSA signmessage）。
///
/// 余额/凭证查询经可注入 [`RpcProbe`]（`with_probe`）；无 probe 时返回 `Err(Internal)`。
pub struct BitcoinAdapter {
    /// 适配器服务的链配置（决定 RPC 端点等）。
    config: crate::model::ChainConfig,
    /// 可选 JSON-RPC 传输探针（余额/凭证查询用；None 时降级为 Err）。
    probe: Option<Box<dyn RpcProbe>>,
}

impl BitcoinAdapter {
    /// 构造（默认链大类 Bitcoin，无 RPC probe）。
    pub fn new(config: crate::model::ChainConfig) -> Self {
        Self {
            config,
            probe: None,
        }
    }

    /// 注入 RPC 传输探针（余额/凭证查询用；RpcRegistryImpl 注册 adapter 时注入）。
    pub fn with_probe(mut self, probe: Box<dyn RpcProbe>) -> Self {
        self.probe = Some(probe);
        self
    }

    /// 校验算法是否属 BTC 大类（纯逻辑路由校验）。
    pub fn accepts(algo: SignatureAlgorithm) -> Result<(), crate::WalletError> {
        if algo.is_bitcoin() {
            Ok(())
        } else {
            Err(crate::WalletError::ChainUnsupported(format!(
                "BitcoinAdapter 不接受非 BTC 算法 {:?}",
                algo
            )))
        }
    }

    /// 取配置（供测试/未来 RPC 调用复用）。
    pub fn config(&self) -> &crate::model::ChainConfig {
        &self.config
    }

    /// 取 RPC 端点 URL（主 → fallback，取第一个非空）。
    fn rpc_url(&self) -> Option<&str> {
        self.config
            .rpc_url
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.config
                    .rpc_fallback_url
                    .as_deref()
                    .filter(|s| !s.is_empty())
            })
    }
}

#[async_trait]
impl ChainAdapter for BitcoinAdapter {
    async fn verify_signature(
        &self,
        address: &AddressId,
        message: &str,
        signature: &[u8],
        algo: SignatureAlgorithm,
    ) -> crate::WalletResult<bool> {
        Self::accepts(algo)?;
        // 按 BTC 算法子类型路由到真实 signing 函数。address 字段语义因算法而异：
        // - Schnorr / ECDSA：传公钥 hex（x-only 32 字节 / 压缩 33 字节），便于直接比对；
        // - BIP-322：传 Bitcoin 地址（bech32/base58check），由 BIP-322 验签解析。
        match algo {
            SignatureAlgorithm::Schnorr => {
                crate::signing::verify_schnorr(message, signature, address.as_str())
            }
            SignatureAlgorithm::Ecdsa => {
                crate::signing::verify_ecdsa_message(message, signature, address.as_str())
            }
            SignatureAlgorithm::Bip322 => {
                crate::signing::verify_bip322(message, signature, address.as_str())
            }
            // accepts() 已保证不会触达非 BTC 分支。
            _ => unreachable!("accepts() 已过滤非 BTC 算法"),
        }
    }

    async fn query_balance(&self, address: &AddressId) -> crate::WalletResult<u128> {
        // 真实 BTC 余额查询：经 JSON-RPC `scantxoutset` 扫描地址相关 UTXO 求和。
        // 缺 probe 时降级为 Err(Internal)。
        let probe = self.probe.as_ref().ok_or_else(|| {
            crate::WalletError::Internal("BTC 余额查询未注入 RpcProbe".to_string())
        })?;
        let url = self.rpc_url().ok_or_else(|| {
            crate::WalletError::RpcUnavailable("BTC 链未配置 RPC URL".to_string())
        })?;
        // scantxoutset: 扫描描述符 addr(<address>) 的所有 UTXO，返回总 satoshis。
        let desc = format!("addr({})", address.as_str());
        let result = probe
            .rpc_call(
                url,
                "scantxoutset",
                serde_json::json!({
                    "action": "start",
                    "scanobjects": [desc],
                }),
            )
            .await?;
        // 解析 result.total_amount（BTC 字符串）→ 转 satoshis。
        let amount_str = result
            .get("total_amount")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                crate::WalletError::Internal(format!(
                    "BTC scantxoutset 响应缺 total_amount: {result}"
                ))
            })?;
        btc_amount_to_sat(amount_str)
    }

    async fn query_credential(
        &self,
        _address: &AddressId,
        cred: CredentialSpec,
    ) -> crate::WalletResult<bool> {
        // 仅 Ordinal 属 BTC；其他凭证规格在此 adapter 下非法。
        if !matches!(cred, CredentialSpec::Ordinal { .. }) {
            return Err(crate::WalletError::ChainUnsupported(format!(
                "BitcoinAdapter 仅查 Ordinal，收到 {:?}",
                cred.chain_kind()
            )));
        }
        // TODO(wallet-agent): 查自托管 ord index（规划文档 §9.1#12），外部 fallback。
        // 当前 ord index 接入未完成，返回 Internal 占位（保持与原 TODO 一致）。
        Err(crate::WalletError::Internal(
            "Ordinal 凭证查询未接入（待 ord index 接入）".to_string(),
        ))
    }

    async fn chain_kind(&self) -> ChainKind {
        ChainKind::Bitcoin
    }
}

/// EVM 链适配器（EIP-191 / EIP-712，ERC-721/1155 凭证）。
///
/// 真实验签基于 alloy + secp256k1（见 [`crate::signing`]）：
/// - [`SignatureAlgorithm::Eip191`]：personal_sign 地址恢复比对。
/// - [`SignatureAlgorithm::Eip712`]：typed_data 哈希 + 地址恢复比对（`message`
///   字段传完整 EIP-712 typed_data JSON 字符串）。
///
/// 余额查询：`eth_getBalance`（wei）。ERC-20/721/1155 凭证：`eth_call`（balanceOf/
/// ownerOf）。无 probe（None）时返回 `Err(Internal)`（优雅降级）。
pub struct EvmAdapter {
    config: crate::model::ChainConfig,
    probe: Option<Box<dyn RpcProbe>>,
}

impl EvmAdapter {
    /// 构造（默认链大类 Evm，无 RPC probe）。
    pub fn new(config: crate::model::ChainConfig) -> Self {
        Self {
            config,
            probe: None,
        }
    }

    /// 注入 RPC 传输探针（余额/凭证查询用）。
    pub fn with_probe(mut self, probe: Box<dyn RpcProbe>) -> Self {
        self.probe = Some(probe);
        self
    }

    /// 校验算法是否属 EVM 大类（纯逻辑路由校验）。
    pub fn accepts(algo: SignatureAlgorithm) -> Result<(), crate::WalletError> {
        if algo.is_evm() {
            Ok(())
        } else {
            Err(crate::WalletError::ChainUnsupported(format!(
                "EvmAdapter 不接受非 EVM 算法 {:?}",
                algo
            )))
        }
    }

    /// 取配置（供测试/未来 RPC 调用复用）。
    pub fn config(&self) -> &crate::model::ChainConfig {
        &self.config
    }

    /// 取 RPC 端点 URL（主 → fallback）。
    fn rpc_url(&self) -> Option<&str> {
        self.config
            .rpc_url
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.config
                    .rpc_fallback_url
                    .as_deref()
                    .filter(|s| !s.is_empty())
            })
    }

    /// 取 RPC probe 引用（余额/凭证查询用；缺则降级）。
    fn probe(&self) -> crate::WalletResult<&dyn RpcProbe> {
        self.probe
            .as_deref()
            .ok_or_else(|| crate::WalletError::Internal("EVM 查询未注入 RpcProbe".to_string()))
    }
}

#[async_trait]
impl ChainAdapter for EvmAdapter {
    async fn verify_signature(
        &self,
        address: &AddressId,
        message: &str,
        signature: &[u8],
        algo: SignatureAlgorithm,
    ) -> crate::WalletResult<bool> {
        Self::accepts(algo)?;
        match algo {
            SignatureAlgorithm::Eip191 => {
                crate::signing::verify_eip191(message, signature, address.as_str())
            }
            SignatureAlgorithm::Eip712 => {
                // EIP-712 的 message 字段承载完整 typed_data JSON。
                crate::signing::verify_eip712(message, signature, address.as_str())
            }
            _ => unreachable!("accepts() 已过滤非 EVM 算法"),
        }
    }

    async fn query_balance(&self, address: &AddressId) -> crate::WalletResult<u128> {
        // EVM 原生余额：eth_getBalance(address, "latest") → wei 十六进制。
        let probe = self.probe()?;
        let url = self.rpc_url().ok_or_else(|| {
            crate::WalletError::RpcUnavailable("EVM 链未配置 RPC URL".to_string())
        })?;
        let result = probe
            .rpc_call(
                url,
                "eth_getBalance",
                serde_json::json!([address.as_str(), "latest"]),
            )
            .await?;
        let hex = result.as_str().ok_or_else(|| {
            crate::WalletError::Internal(format!("eth_getBalance 响应非字符串: {result}"))
        })?;
        parse_u128_hex(hex)
    }

    async fn query_credential(
        &self,
        address: &AddressId,
        cred: CredentialSpec,
    ) -> crate::WalletResult<bool> {
        // 仅 ERC-721 / ERC-1155 属 EVM。
        let (contract, token_id) = match &cred {
            CredentialSpec::Erc721 { contract, token_id }
            | CredentialSpec::Erc1155 { contract, token_id } => {
                (contract.clone(), token_id.clone())
            }
            other => {
                return Err(crate::WalletError::ChainUnsupported(format!(
                    "EvmAdapter 仅查 ERC-721/1155，收到 {:?} 链凭证",
                    other.chain_kind()
                )));
            }
        };
        // ERC-721 ownerOf(tokenId) == address ?
        // ERC-1155 balanceOf(address, tokenId) > 0 ?
        // 这里以 ownerOf（ERC-721）/ balanceOf（ERC-1155）两种 call 实现，
        // 都用 eth_call（to=contract, data=selector+encoded args）。
        let probe = self.probe()?;
        let url = self.rpc_url().ok_or_else(|| {
            crate::WalletError::RpcUnavailable("EVM 链未配置 RPC URL".to_string())
        })?;
        // 构造 calldata（4 字节 selector + ABI 编码参数）。
        let (selector, calldata_args) = match &cred {
            CredentialSpec::Erc721 { .. } => {
                // ownerOf(uint256) selector = keccak256("ownerOf(uint256)")[:4]
                let selector = eth_function_selector("ownerOf(uint256)");
                let args = eth_encode_address_and_uint256(address.as_str(), &token_id)?;
                (selector, args)
            }
            CredentialSpec::Erc1155 { .. } => {
                // balanceOf(address,uint256) selector
                let selector = eth_function_selector("balanceOf(address,uint256)");
                let args = eth_encode_address_and_uint256(address.as_str(), &token_id)?;
                (selector, args)
            }
            _ => unreachable!("上方已过滤非 EVM 凭证"),
        };
        let mut data = Vec::with_capacity(selector.len() + calldata_args.len());
        data.extend_from_slice(&selector);
        data.extend_from_slice(&calldata_args);
        let data_hex = format!("0x{}", alloy::hex::encode(&data));
        let result = probe
            .rpc_call(
                url,
                "eth_call",
                serde_json::json!([
                    {"to": contract, "data": data_hex},
                    "latest"
                ]),
            )
            .await?;
        let ret_hex = result.as_str().ok_or_else(|| {
            crate::WalletError::Internal(format!("eth_call 响应非字符串: {result}"))
        })?;
        match &cred {
            CredentialSpec::Erc721 { .. } => {
                // ownerOf 返回 address（左 padding 到 32 字节）。
                let bytes = crate::signing::decode_hex_maybe_0x(ret_hex)?;
                if bytes.len() < 32 {
                    return Ok(false);
                }
                let owner = &bytes[12..32];
                let owner_hex = format!("0x{}", alloy::hex::encode(owner));
                Ok(owner_hex.eq_ignore_ascii_case(address.as_str()))
            }
            CredentialSpec::Erc1155 { .. } => {
                // balanceOf 返回 uint256；> 0 即持有。
                let val = parse_u128_hex(ret_hex).unwrap_or(0);
                Ok(val > 0)
            }
            _ => unreachable!(),
        }
    }

    async fn chain_kind(&self) -> ChainKind {
        ChainKind::Evm
    }
}

// ============================================================================
// ABI 编码辅助（纯逻辑，无外部 sol-types 依赖；专用于 eth_call calldata 构造）
// ============================================================================

/// 计算 Solidity 函数选择器：`keccak256(signature)[:4]`。
///
/// 用 alloy 的 keccak256（与 alloy_primitives 同源），与 on-chain selector 一致。
fn eth_function_selector(signature: &str) -> [u8; 4] {
    let hash = alloy::primitives::keccak256(signature.as_bytes());
    let mut out = [0u8; 4];
    out.copy_from_slice(&hash[..4]);
    out
}

/// 编码 calldata 参数：address（左 padding 到 32 字节）+ uint256（token_id）。
///
/// 适用于 ownerOf(uint256) / balanceOf(address,uint256)。
/// token_id 接受十进制或 `0x` 十六进制字符串。
fn eth_encode_address_and_uint256(address: &str, token_id: &str) -> crate::WalletResult<Vec<u8>> {
    let mut out = Vec::with_capacity(64);
    // address → 20 字节（去 `0x` 后 hex 解码）。
    let addr_bytes = crate::signing::decode_hex_maybe_0x(address)?;
    if addr_bytes.len() != 20 {
        return Err(crate::WalletError::ChainUnsupported(format!(
            "EVM 地址长度非 20 字节: {}",
            addr_bytes.len()
        )));
    }
    // 左 padding 到 32 字节。
    out.extend_from_slice(&[0u8; 12]);
    out.extend_from_slice(&addr_bytes);
    // uint256 token_id。
    let id_bytes = encode_uint256(token_id)?;
    out.extend_from_slice(&id_bytes);
    Ok(out)
}

/// 把 token_id 字符串（十进制或 `0x` hex）编码为 32 字节大端。
fn encode_uint256(token_id: &str) -> crate::WalletResult<[u8; 32]> {
    let val: u128 = if let Some(hex) = token_id
        .strip_prefix("0x")
        .or_else(|| token_id.strip_prefix("0X"))
    {
        u128::from_str_radix(hex, 16)
            .map_err(|e| crate::WalletError::Internal(format!("token_id hex 解析失败: {e}")))?
    } else {
        token_id
            .parse::<u128>()
            .map_err(|e| crate::WalletError::Internal(format!("token_id 十进制解析失败: {e}")))?
    };
    let mut out = [0u8; 32];
    out[16..].copy_from_slice(&val.to_be_bytes());
    Ok(out)
}

/// 把 hex 字符串（`0x...`）解析为 u128（用于 eth_getBalance / eth_call uint256 返回）。
///
/// 内部直接用 u128::from_str_radix，避免 hex 字节解码对奇数长度的拒绝（`0x5` 等单 nibble
/// 合法）。超过 u128 上界的高位被丢弃（取低 128 位）。
fn parse_u128_hex(hex: &str) -> crate::WalletResult<u128> {
    let stripped = hex.strip_prefix("0x").unwrap_or(hex);
    if stripped.is_empty() {
        return Ok(0);
    }
    // u128 直接解析；若超 u128（uint256 高位非零）则按低 128 位取（u256 mod 2^128）。
    // 实现上：先解析低 32 hex（u128），失败再尝试更高位 → 取模。
    if let Ok(v) = u128::from_str_radix(stripped, 16) {
        return Ok(v);
    }
    // 长 hex（uint256）：取末 32 hex（128 位）。
    let tail_len = stripped.len().min(32);
    let tail = &stripped[stripped.len() - tail_len..];
    Ok(u128::from_str_radix(tail, 16).unwrap_or(0))
}

/// 把 BTC 金额字符串（如 "1.5" BTC）转为 satoshis（u128）。
///
/// 纯字符串/小数解析；超过 u128 上界或解析失败抛 Internal。
fn btc_amount_to_sat(amount: &str) -> crate::WalletResult<u128> {
    let mut parts = amount.splitn(2, '.');
    let int_part = parts.next().unwrap_or("0");
    let frac_part = parts.next().unwrap_or("");
    let int_val: u128 = int_part
        .parse()
        .map_err(|e| crate::WalletError::Internal(format!("BTC 整数部分解析失败: {e}")))?;
    // 小数部分补/截到 8 位（1 BTC = 1e8 sat）。
    let mut frac = frac_part.to_string();
    if frac.len() > 8 {
        frac.truncate(8);
    } else {
        while frac.len() < 8 {
            frac.push('0');
        }
    }
    let frac_val: u128 = frac
        .parse()
        .map_err(|e| crate::WalletError::Internal(format!("BTC 小数部分解析失败: {e}")))?;
    int_val
        .checked_mul(100_000_000)
        .and_then(|v| v.checked_add(frac_val))
        .ok_or_else(|| crate::WalletError::Internal("BTC 金额超 u128 上界".to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ChainConfig;
    use os_core::AddressId;

    fn btc_cfg() -> ChainConfig {
        ChainConfig::new("bitcoin", ChainKind::Bitcoin, "http://localhost:8332")
    }

    fn evm_cfg() -> ChainConfig {
        ChainConfig::new("ethereum", ChainKind::Evm, "http://localhost:8545")
    }

    #[test]
    fn adapter_accepts_correct_algorithm() {
        assert!(BitcoinAdapter::accepts(SignatureAlgorithm::Bip322).is_ok());
        assert!(BitcoinAdapter::accepts(SignatureAlgorithm::Eip191).is_err());
        assert!(EvmAdapter::accepts(SignatureAlgorithm::Eip712).is_ok());
        assert!(EvmAdapter::accepts(SignatureAlgorithm::Schnorr).is_err());
    }

    #[tokio::test]
    async fn bitcoin_adapter_routes_and_rejects_non_btc() {
        let a = BitcoinAdapter::new(btc_cfg());
        assert_eq!(a.chain_kind().await, ChainKind::Bitcoin);

        // 非 BTC 算法被路由校验拒绝（ChainUnsupported），不进验签分支。
        let err = a
            .verify_signature(
                &AddressId::new("bc1q"),
                "msg",
                &[0u8; 64],
                SignatureAlgorithm::Eip191,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, crate::WalletError::ChainUnsupported(_)));

        // Schnorr 算法通过路由 → 真实验签：全零签名 → Ok(false)（验签失败但不抛错）。
        // （全零签名为非法 BIP-340 签名，verify_schnorr 返回 Ok(false)。）
        let r = a
            .verify_signature(
                &AddressId::new("00".repeat(32)),
                "msg",
                &[0u8; 64],
                SignatureAlgorithm::Schnorr,
            )
            .await;
        // 全零公钥可能解析失败（Err）或验签失败（Ok(false)），两者都不是 ChainUnsupported。
        assert!(r.is_ok() || matches!(r, Err(crate::WalletError::SignatureInvalid(_))));

        // 非 Ordinal 凭证在 BTC adapter 下拒绝。
        let err = a
            .query_credential(
                &AddressId::new("bc1q"),
                CredentialSpec::Erc721 {
                    contract: "0x".into(),
                    token_id: "1".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, crate::WalletError::ChainUnsupported(_)));
    }

    #[tokio::test]
    async fn evm_adapter_routes_and_rejects_non_evm() {
        let a = EvmAdapter::new(evm_cfg());
        assert_eq!(a.chain_kind().await, ChainKind::Evm);
        assert_eq!(a.config().kind, ChainKind::Evm);

        // 非 EVM 算法被拒绝。
        let err = a
            .verify_signature(
                &AddressId::new("0xabc"),
                "msg",
                &[0u8; 65],
                SignatureAlgorithm::Bip322,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, crate::WalletError::ChainUnsupported(_)));

        // EIP-191 通过路由 → 真实验签：错误短地址解析失败（SignatureInvalid）。
        let err = a
            .verify_signature(
                &AddressId::new("0xabc"),
                "Hello",
                b"0xbad".as_ref(),
                SignatureAlgorithm::Eip191,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, crate::WalletError::SignatureInvalid(_)));

        // Ordinal 凭证在 EVM adapter 下拒绝。
        let err = a
            .query_credential(
                &AddressId::new("0xabc"),
                CredentialSpec::Ordinal {
                    inscription_id: "i".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, crate::WalletError::ChainUnsupported(_)));
    }

    // ---- 真实验签：通过 adapter 走 RFC EIP-191 向量 ----

    #[tokio::test]
    async fn evm_adapter_verify_eip191_rfc_vector() {
        let a = EvmAdapter::new(evm_cfg());
        let sig_hex = "b91467e570a6466aa9e9876cbcd013baba02900b8979d43fe208a4a4f339f5fd6007e74cd82e037b800186422fc2da167c747ef045e5d18a5f5d4300f8e1a0291c";
        let addr = "0x2c7536E3605D9C16a7a3D7b1898e529396a65c23";
        assert!(
            a.verify_signature(
                &AddressId::new(addr),
                "Some data",
                sig_hex.as_bytes(),
                SignatureAlgorithm::Eip191,
            )
            .await
            .unwrap(),
            "EvmAdapter EIP-191 RFC 向量应通过"
        );

        // 错地址 → Ok(false)。
        assert!(!a
            .verify_signature(
                &AddressId::new("0x0000000000000000000000000000000000000001"),
                "Some data",
                sig_hex.as_bytes(),
                SignatureAlgorithm::Eip191,
            )
            .await
            .unwrap());
    }

    // ---- 真实查询：余额/凭证经 FixtureProbe（fixture 测，零网络）----

    /// 简单内存 probe：按 method 名返回固定 JSON。
    struct StaticProbe {
        responses: std::collections::HashMap<String, serde_json::Value>,
    }
    #[async_trait]
    impl RpcProbe for StaticProbe {
        async fn rpc_call(
            &self,
            _url: &str,
            method: &str,
            _params: serde_json::Value,
        ) -> WalletResult<serde_json::Value> {
            self.responses
                .get(method)
                .cloned()
                .ok_or_else(|| crate::WalletError::RpcUnavailable(format!("未配置 {method}")))
        }
    }

    #[tokio::test]
    async fn evm_adapter_query_balance_via_probe() {
        // eth_getBalance 返回 1 ETH = 1e18 wei（hex）。
        let probe = StaticProbe {
            responses: std::collections::HashMap::from([(
                "eth_getBalance".to_string(),
                serde_json::json!("0x0de0b6b3a7640000"),
            )]),
        };
        let a = EvmAdapter::new(evm_cfg()).with_probe(Box::new(probe));
        let bal = a
            .query_balance(&AddressId::new(
                "0x0000000000000000000000000000000000000001",
            ))
            .await
            .unwrap();
        assert_eq!(bal, 1_000_000_000_000_000_000u128, "应等于 1 ETH (wei)");

        // 无 probe → Err(Internal)。
        let a2 = EvmAdapter::new(evm_cfg());
        let err = a2.query_balance(&AddressId::new("0x..")).await.unwrap_err();
        assert!(matches!(err, crate::WalletError::Internal(_)));
    }

    #[tokio::test]
    async fn evm_adapter_query_erc721_owner_match() {
        // ownerOf 返回 owner = 0x..0001（左 padding 到 32 字节）。
        let owner_ret = format!(
            "0x{}{}",
            "0".repeat(24),
            "0000000000000000000000000000000000000001"
        );
        let probe = StaticProbe {
            responses: std::collections::HashMap::from([(
                "eth_call".to_string(),
                serde_json::json!(owner_ret),
            )]),
        };
        let a = EvmAdapter::new(evm_cfg()).with_probe(Box::new(probe));
        let held = a
            .query_credential(
                &AddressId::new("0x0000000000000000000000000000000000000001"),
                CredentialSpec::Erc721 {
                    contract: "0x0123456789012345678901234567890123456789".into(),
                    token_id: "42".into(),
                },
            )
            .await
            .unwrap();
        assert!(held, "owner 匹配应判 held=true");

        // 不匹配地址 → false。
        let held2 = EvmAdapter::new(evm_cfg())
            .with_probe(Box::new(StaticProbe {
                responses: std::collections::HashMap::from([(
                    "eth_call".to_string(),
                    serde_json::json!(owner_ret),
                )]),
            }))
            .query_credential(
                &AddressId::new("0x0000000000000000000000000000000000000002"),
                CredentialSpec::Erc721 {
                    contract: "0x0123456789012345678901234567890123456789".into(),
                    token_id: "42".into(),
                },
            )
            .await
            .unwrap();
        assert!(!held2);
    }

    #[tokio::test]
    async fn evm_adapter_query_erc1155_balance() {
        // balanceOf 返回 5（uint256）。
        let probe = StaticProbe {
            responses: std::collections::HashMap::from([(
                "eth_call".to_string(),
                serde_json::json!(format!("0x{}{}", "0".repeat(63), "5")),
            )]),
        };
        let a = EvmAdapter::new(evm_cfg()).with_probe(Box::new(probe));
        let held = a
            .query_credential(
                &AddressId::new("0x0000000000000000000000000000000000000001"),
                CredentialSpec::Erc1155 {
                    contract: "0x0123456789012345678901234567890123456789".into(),
                    token_id: "7".into(),
                },
            )
            .await
            .unwrap();
        assert!(held, "balance=5 > 0 应判 held=true");
    }

    #[tokio::test]
    async fn bitcoin_adapter_query_balance_via_probe() {
        // scantxoutset 返回 total_amount = "0.5" BTC = 5e7 sat。
        let probe = StaticProbe {
            responses: std::collections::HashMap::from([(
                "scantxoutset".to_string(),
                serde_json::json!({"total_amount": "0.5"}),
            )]),
        };
        let a = BitcoinAdapter::new(btc_cfg()).with_probe(Box::new(probe));
        let bal = a
            .query_balance(&AddressId::new(
                "bc1qar0srrr7xfkvy5l643lydnw9re59gtzzwf5mdq",
            ))
            .await
            .unwrap();
        assert_eq!(bal, 50_000_000, "0.5 BTC = 5e7 sat");
    }

    // ---- ABI 编码辅助测试 ----

    #[test]
    fn eth_function_selector_known_values() {
        // ownerOf(uint256) = 0x6352211e
        assert_eq!(
            eth_function_selector("ownerOf(uint256)"),
            [0x63, 0x52, 0x21, 0x1e]
        );
        // balanceOf(address,uint256) = 0x00fdd58e
        assert_eq!(
            eth_function_selector("balanceOf(address,uint256)"),
            [0x00, 0xfd, 0xd5, 0x8e]
        );
    }

    #[test]
    fn parse_u128_hex_roundtrip() {
        assert_eq!(parse_u128_hex("0x0").unwrap(), 0);
        assert_eq!(
            parse_u128_hex("0x0de0b6b3a7640000").unwrap(),
            1_000_000_000_000_000_000
        );
        assert_eq!(parse_u128_hex("0x5").unwrap(), 5);
    }

    #[test]
    fn btc_amount_to_sat_known_values() {
        assert_eq!(btc_amount_to_sat("0").unwrap(), 0);
        assert_eq!(btc_amount_to_sat("1").unwrap(), 100_000_000);
        assert_eq!(btc_amount_to_sat("0.5").unwrap(), 50_000_000);
        assert_eq!(btc_amount_to_sat("0.00000001").unwrap(), 1);
        assert_eq!(btc_amount_to_sat("21").unwrap(), 2_100_000_000);
    }
}
