//! Mock 实现（仅 `mock` feature 下编译）。
//!
//! 提供 [`MockWalletConnector`] / [`MockChainAdapter`] / [`MockRpcRegistry`] /
//! [`FixtureProbe`](crate::mock::FixtureProbe)，供下游 agent（guest-agent /
//! im-agent）单元测/集成测注入。
//!
//! [`FixtureProbe`](crate::mock::FixtureProbe) 是 [`RpcProbe`](crate::registry::RpcProbe)
//! 的内存实现——按 (method) 表返回固定 JSON 响应，零网络，专用于验证
//! `RpcRegistryImpl` 的探活/降级路径。
//!
//! 行为：纯内存、确定性。构造器 `MockXxx::new().with_*()` 设置预期返回值；
//! 未配置时返回安全的默认值（不 panic）。

use async_trait::async_trait;
use os_core::{AddressId, WalletSessionId};
use std::collections::HashMap;
use std::sync::Mutex;

use crate::chain::{ChainAdapter, CredentialSpec};
use crate::connector::{ConnectorKind, SignRequest, SignResponse, WalletConnector, WalletSession};
use crate::model::{ChainKind, SignatureAlgorithm};
use crate::registry::{RpcProbe, RpcRegistry, RpcResult, RpcSource, RpcStatus};
use crate::WalletResult;

// ============================================================================
// MockWalletConnector
// ============================================================================

/// Mock 钱包连接器。
///
/// 默认：connect 返回一个有效会话；disconnect 按表删除；request_signature
/// 返回全零 65 字节签名 + 配置地址。可通过 `with_*` 覆盖。
pub struct MockWalletConnector {
    sessions: Mutex<HashMap<WalletSessionId, WalletSession>>,
    /// 用于签名的固定地址（默认 0x0）。
    sign_address: AddressId,
    /// connect 失败开关（设 true 则 connect 返回 Internal）。
    connect_fails: bool,
}

impl MockWalletConnector {
    /// 构造默认 mock。
    pub fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            sign_address: AddressId::new("0x0000000000000000000000000000000000000000"),
            connect_fails: false,
        }
    }

    /// 设置签名返回地址。
    pub fn with_sign_address(mut self, addr: impl Into<String>) -> Self {
        self.sign_address = AddressId::new(addr);
        self
    }

    /// 设置 connect 是否失败。
    pub fn with_connect_failing(mut self, fails: bool) -> Self {
        self.connect_fails = fails;
        self
    }

    /// 直接注入一个会话（供测试预置）。
    pub fn inject_session(&self, session: WalletSession) -> WalletSessionId {
        let id = session.id.clone();
        self.sessions
            .lock()
            .expect("mock sessions")
            .insert(id.clone(), session);
        id
    }
}

impl Default for MockWalletConnector {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl WalletConnector for MockWalletConnector {
    async fn connect(&self, chain: ChainKind, kind: ConnectorKind) -> WalletResult<WalletSession> {
        if self.connect_fails {
            return Err(crate::WalletError::ConnectFailed(
                "mock connect 失败开关已打开".to_string(),
            ));
        }
        let session =
            WalletSession::new(chain, self.sign_address.clone(), kind, chrono::Utc::now());
        self.sessions
            .lock()
            .expect("mock sessions")
            .insert(session.id.clone(), session.clone());
        Ok(session)
    }

    async fn disconnect(&self, session: &WalletSessionId) -> WalletResult<()> {
        let removed = self
            .sessions
            .lock()
            .expect("mock sessions")
            .remove(session)
            .is_some();
        if removed {
            Ok(())
        } else {
            Err(crate::WalletError::SessionNotFound(session.to_string()))
        }
    }

    async fn request_signature(&self, req: SignRequest) -> WalletResult<SignResponse> {
        // 默认返回 65 字节零签名（EVM ECDSA 长度），地址为配置地址。
        // 不校验会话存在性（mock 保持简单，真实校验由具体 Connector 完成）。
        let _ = req.message.len();
        Ok(SignResponse {
            signature: vec![0u8; 65],
            address: self.sign_address.clone(),
        })
    }

    async fn list_sessions(&self) -> WalletResult<Vec<WalletSession>> {
        Ok(self
            .sessions
            .lock()
            .expect("mock sessions")
            .values()
            .cloned()
            .collect())
    }
}

// ============================================================================
// MockChainAdapter
// ============================================================================

/// Mock 链适配器。
///
/// 默认：verify_signature 返回 true；query_balance 返回 0；query_credential
/// 返回 false；chain_kind 返回构造时配置。可通过 `with_*` 覆盖。
pub struct MockChainAdapter {
    chain: ChainKind,
    verify_result: bool,
    balance: u128,
    credentials_held: Mutex<HashMap<String, bool>>,
}

impl MockChainAdapter {
    /// 构造指定链大类的 mock。
    pub fn new(chain: ChainKind) -> Self {
        Self {
            chain,
            verify_result: true,
            balance: 0,
            credentials_held: Mutex::new(HashMap::new()),
        }
    }

    /// 设置验签结果。
    pub fn with_verify_result(mut self, ok: bool) -> Self {
        self.verify_result = ok;
        self
    }

    /// 设置余额。
    pub fn with_balance(mut self, balance: u128) -> Self {
        self.balance = balance;
        self
    }

    /// 预置某地址持有某凭证（key 用 `inscription_id` 或 `contract:token_id`）。
    pub fn with_credential_held(self, key: impl Into<String>) -> Self {
        self.credentials_held
            .lock()
            .expect("mock creds")
            .insert(key.into(), true);
        self
    }

    fn credential_key(cred: &CredentialSpec) -> String {
        match cred {
            CredentialSpec::Ordinal { inscription_id } => format!("ordinal:{inscription_id}"),
            CredentialSpec::Erc721 { contract, token_id } => {
                format!("erc721:{contract}:{token_id}")
            }
            CredentialSpec::Erc1155 { contract, token_id } => {
                format!("erc1155:{contract}:{token_id}")
            }
        }
    }
}

#[async_trait]
impl ChainAdapter for MockChainAdapter {
    async fn verify_signature(
        &self,
        _address: &AddressId,
        _message: &str,
        _signature: &[u8],
        _algo: SignatureAlgorithm,
    ) -> WalletResult<bool> {
        Ok(self.verify_result)
    }

    async fn query_balance(&self, _address: &AddressId) -> WalletResult<u128> {
        Ok(self.balance)
    }

    async fn query_credential(
        &self,
        _address: &AddressId,
        cred: CredentialSpec,
    ) -> WalletResult<bool> {
        let key = Self::credential_key(&cred);
        Ok(self
            .credentials_held
            .lock()
            .expect("mock creds")
            .get(&key)
            .copied()
            .unwrap_or(false))
    }

    async fn chain_kind(&self) -> ChainKind {
        self.chain
    }
}

// ============================================================================
// MockRpcRegistry
// ============================================================================

/// Mock RPC 注册表。
///
/// 默认：所有链 available=true；check 返回固定 RpcStatus。可通过
/// `set_available` 切换。adapter 注册表为内存态，register/unregister 不报错。
pub struct MockRpcRegistry {
    available: Mutex<HashMap<ChainKind, bool>>,
    adapters: Mutex<HashMap<ChainKind, Box<dyn ChainAdapter>>>,
}

impl MockRpcRegistry {
    /// 构造默认 mock（Bitcoin + Evm 均可用）。
    pub fn new() -> Self {
        let mut avail = HashMap::new();
        avail.insert(ChainKind::Bitcoin, true);
        avail.insert(ChainKind::Evm, true);
        Self {
            available: Mutex::new(avail),
            adapters: Mutex::new(HashMap::new()),
        }
    }

    /// 设置某链可用性。
    pub fn set_available(&self, chain: ChainKind, available: bool) {
        self.available
            .lock()
            .expect("mock avail")
            .insert(chain, available);
    }

    /// 取某链可用性（缺省 false）。
    pub fn is_set_available(&self, chain: ChainKind) -> bool {
        self.available
            .lock()
            .expect("mock avail")
            .get(&chain)
            .copied()
            .unwrap_or(false)
    }
}

impl Default for MockRpcRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RpcRegistry for MockRpcRegistry {
    async fn check(&self, chain: ChainKind) -> WalletResult<RpcStatus> {
        let available = self.is_set_available(chain);
        Ok(RpcStatus {
            chain,
            available,
            latency_ms: if available { Some(1) } else { None },
            last_check: chrono::Utc::now(),
            source: RpcSource::Local,
        })
    }

    async fn check_all(&self) -> WalletResult<Vec<RpcStatus>> {
        let chains = vec![ChainKind::Bitcoin, ChainKind::Evm];
        let mut out = Vec::new();
        for c in chains {
            out.push(self.check(c).await?);
        }
        Ok(out)
    }

    async fn is_available(&self, chain: ChainKind) -> WalletResult<bool> {
        Ok(self.is_set_available(chain))
    }

    async fn register_adapter(&self, adapter: Box<dyn ChainAdapter>) -> WalletResult<()> {
        let chain = adapter.chain_kind().await;
        self.adapters
            .lock()
            .expect("mock adapters")
            .insert(chain, adapter);
        Ok(())
    }

    async fn unregister_adapter(&self, chain: ChainKind) -> WalletResult<()> {
        self.adapters.lock().expect("mock adapters").remove(&chain);
        Ok(())
    }
}

// ============================================================================
// FixtureProbe —— RpcProbe 的内存 fixture 实现（零网络，单元测专用）
// ============================================================================

/// `RpcProbe` 的内存 fixture：按 JSON-RPC method 名返回预置响应。
///
/// 专用于 `RpcRegistryImpl` 探活/降级路径的单测——构造时 `with_method` 注册每个
/// method 的响应（成功 result / 错误 / 缺字段），探针据此原样返回，不发任何网络。
///
/// # 示例
/// ```ignore
/// use os_wallet::mock::FixtureProbe;
/// use os_wallet::registry::RpcRegistryImpl;
///
/// let probe = FixtureProbe::new()
///     .with_method("eth_blockNumber", serde_json::json!("0x10"));
/// let reg = RpcRegistryImpl::new(vec![])
///     .with_probe(probe);
/// ```
pub struct FixtureProbe {
    /// method → 响应。响应为 Ok(value) 表示成功返回该 result 字段；
    /// Err(msg) 表示模拟一次失败（连接/超时/HTTP 错误等）。
    responses: Mutex<HashMap<String, FixtureResponse>>,
    /// 记录每次 rpc_call 的 (url, method)（供测试断言探活命中的端点）。
    calls: Mutex<Vec<(String, String)>>,
}

/// 单次 fixture 响应。
#[derive(Clone, Debug)]
pub enum FixtureResponse {
    /// 成功：返回此值作为 JSON-RPC `result`。
    Ok(serde_json::Value),
    /// 失败：模拟一次 RPC 错误（如连接拒绝/超时）。
    Err(String),
}

impl FixtureProbe {
    /// 构造空 fixture（所有 method 默认返回 "未配置" 错误）。
    pub fn new() -> Self {
        Self {
            responses: Mutex::new(HashMap::new()),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// 注册某 method 的成功响应（作为 JSON-RPC `result` 字段）。
    pub fn with_method(self, method: impl Into<String>, result: serde_json::Value) -> Self {
        self.responses
            .lock()
            .expect("fixture responses")
            .insert(method.into(), FixtureResponse::Ok(result));
        self
    }

    /// 注册某 method 的失败响应（模拟连接/超时/HTTP 错误）。
    pub fn with_method_error(self, method: impl Into<String>, msg: impl Into<String>) -> Self {
        self.responses
            .lock()
            .expect("fixture responses")
            .insert(method.into(), FixtureResponse::Err(msg.into()));
        self
    }

    /// 取已记录的调用（按 (url, method)），供测试断言。
    pub fn calls(&self) -> Vec<(String, String)> {
        self.calls.lock().expect("fixture calls").clone()
    }
}

impl Default for FixtureProbe {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RpcProbe for FixtureProbe {
    async fn rpc_call(
        &self,
        url: &str,
        method: &str,
        _params: serde_json::Value,
    ) -> WalletResult<RpcResult> {
        self.calls
            .lock()
            .expect("fixture calls")
            .push((url.to_string(), method.to_string()));
        match self
            .responses
            .lock()
            .expect("fixture responses")
            .get(method)
        {
            Some(FixtureResponse::Ok(v)) => Ok(v.clone()),
            Some(FixtureResponse::Err(msg)) => Err(crate::WalletError::RpcUnavailable(msg.clone())),
            None => Err(crate::WalletError::RpcUnavailable(format!(
                "fixture 未配置 method `{method}` 的响应"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn wallet_connector_mock_lifecycle() {
        let c = MockWalletConnector::new().with_sign_address("0xdeadbeef");
        let s = c
            .connect(ChainKind::Evm, ConnectorKind::WalletConnectV2)
            .await
            .unwrap();
        assert_eq!(s.address.as_str(), "0xdeadbeef");
        assert_eq!(c.list_sessions().await.unwrap().len(), 1);

        let resp = c
            .request_signature(SignRequest::new(
                s.id.clone(),
                "hi",
                SignatureAlgorithm::Eip191,
            ))
            .await
            .unwrap();
        assert_eq!(resp.signature.len(), 65);

        c.disconnect(&s.id).await.unwrap();
        let err = c.disconnect(&s.id).await.unwrap_err();
        assert!(matches!(err, crate::WalletError::SessionNotFound(_)));

        // connect 失败开关。
        let failing = MockWalletConnector::new().with_connect_failing(true);
        let err = failing
            .connect(ChainKind::Evm, ConnectorKind::Injected)
            .await
            .unwrap_err();
        assert!(matches!(err, crate::WalletError::ConnectFailed(_)));
    }

    #[tokio::test]
    async fn chain_adapter_mock_defaults_and_overrides() {
        let a = MockChainAdapter::new(ChainKind::Evm);
        assert_eq!(a.chain_kind().await, ChainKind::Evm);
        assert!(a
            .verify_signature(
                &AddressId::new("0x1"),
                "m",
                &[0u8; 65],
                SignatureAlgorithm::Eip191
            )
            .await
            .unwrap());
        assert_eq!(a.query_balance(&AddressId::new("0x1")).await.unwrap(), 0);
        assert!(!a
            .query_credential(
                &AddressId::new("0x1"),
                CredentialSpec::Erc721 {
                    contract: "0xc".into(),
                    token_id: "7".into(),
                }
            )
            .await
            .unwrap());

        let b = MockChainAdapter::new(ChainKind::Bitcoin)
            .with_verify_result(false)
            .with_balance(1_000)
            .with_credential_held("ordinal:abc");
        assert!(!b
            .verify_signature(
                &AddressId::new("bc1q"),
                "m",
                &[0u8; 64],
                SignatureAlgorithm::Bip322
            )
            .await
            .unwrap());
        assert_eq!(
            b.query_balance(&AddressId::new("bc1q")).await.unwrap(),
            1_000
        );
        assert!(b
            .query_credential(
                &AddressId::new("bc1q"),
                CredentialSpec::Ordinal {
                    inscription_id: "abc".into()
                }
            )
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn rpc_registry_mock_default_and_toggle() {
        let r = MockRpcRegistry::new();
        assert!(r.is_available(ChainKind::Bitcoin).await.unwrap());
        assert!(r.is_available(ChainKind::Evm).await.unwrap());

        r.set_available(ChainKind::Evm, false);
        assert!(!r.is_available(ChainKind::Evm).await.unwrap());

        let all = r.check_all().await.unwrap();
        assert_eq!(all.len(), 2);
        let evm_status = all.iter().find(|s| s.chain == ChainKind::Evm).unwrap();
        assert!(!evm_status.available);

        // register/unregister 无报错。
        let adapter = MockChainAdapter::new(ChainKind::Bitcoin);
        r.register_adapter(Box::new(adapter)).await.unwrap();
        r.unregister_adapter(ChainKind::Bitcoin).await.unwrap();
    }

    #[tokio::test]
    async fn fixture_probe_dispatches_and_records() {
        let p = FixtureProbe::new()
            .with_method("eth_blockNumber", serde_json::json!("0x10"))
            .with_method_error("getblockchaininfo", "连接拒绝");
        // 命中成功响应。
        let r = p
            .rpc_call("http://evm", "eth_blockNumber", serde_json::Value::Null)
            .await
            .unwrap();
        assert_eq!(r, serde_json::json!("0x10"));
        // 命中失败响应。
        let err = p
            .rpc_call("http://btc", "getblockchaininfo", serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(matches!(err, crate::WalletError::RpcUnavailable(_)));
        // 未配置 method 报错。
        let err = p
            .rpc_call("http://x", "net_version", serde_json::Value::Null)
            .await
            .unwrap_err();
        assert!(matches!(err, crate::WalletError::RpcUnavailable(_)));
        // 调用记录按顺序保存。
        let calls = p.calls();
        assert_eq!(
            calls,
            vec![
                ("http://evm".to_string(), "eth_blockNumber".to_string()),
                ("http://btc".to_string(), "getblockchaininfo".to_string()),
                ("http://x".to_string(), "net_version".to_string()),
            ]
        );
    }
}
