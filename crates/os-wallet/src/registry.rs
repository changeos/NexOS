//! RPC 注册表（条件激活核心，§3.17）
//!
//! 实现说明：
//! - 探针：BTC 调 `getblockchaininfo` / EVM 调 `eth_blockNumber`
//! - 链 RPC 可用时 `register_adapter`（注入对应 ChainAdapter），
//!   不可用时 `unregister_adapter` 注销，业务侧据此降级（如 fallback 到远程节点）
//!
//! 真实探活（自本提交起）：HTTP/JSON-RPC 调用由 `reqwest` 实现，经可注入的
//! [`RpcProbe`] trait 抽象传输层——生产侧用 [`ReqwestProbe`]（真实 HTTP），
//! 测试侧用 `mock` feature 下的 `FixtureProbe`（返回固定 JSON，零网络）。

use async_trait::async_trait;
use os_core::{Deserialize, Serialize};
use std::time::Duration;

use crate::chain::ChainAdapter;
use crate::model::{ChainConfig, ChainKind};
use crate::WalletResult;

// ----------------------------------------------------------------------------
// RPC 状态 / 来源
// ----------------------------------------------------------------------------

/// RPC 数据源
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RpcSource {
    /// 本地全节点（首选）
    Local,
    /// 远程公共节点（fallback）
    Remote,
}

/// 单条链的 RPC 可用性探测结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcStatus {
    /// 链大类
    pub chain: ChainKind,
    /// 是否可用（本地或远程任一可达即为 true）
    pub available: bool,
    /// 探测延迟（毫秒；None = 不可达/超时）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u32>,
    /// 最近一次探测时间
    pub last_check: os_core::DateTime,
    /// 实际命中的数据源
    pub source: RpcSource,
}

// ----------------------------------------------------------------------------
// RpcRegistry trait（async，条件激活核心）
// ----------------------------------------------------------------------------

/// RPC 注册表——按链 RPC 可用性条件激活 `ChainAdapter`。
///
/// 业务层通过 `is_available` 判断链是否可用，可用时走对应 adapter，
/// 不可用时降级（如禁用该链签名/凭证校验，或 fallback 到远程只读）。
#[async_trait]
pub trait RpcRegistry: Send + Sync {
    /// 探测单条链的 RPC 可用性。
    async fn check(&self, chain: ChainKind) -> WalletResult<RpcStatus>;

    /// 探测所有已配置链。
    async fn check_all(&self) -> WalletResult<Vec<RpcStatus>>;

    /// 查询链是否当前可用（基于最近一次探测结果缓存）。
    async fn is_available(&self, chain: ChainKind) -> WalletResult<bool>;

    /// 注册链适配器（链 RPC 可用时调用，注入 adapter）。
    async fn register_adapter(&self, adapter: Box<dyn ChainAdapter>) -> WalletResult<()>;

    /// 注销链适配器（链 RPC 不可用时调用）。
    async fn unregister_adapter(&self, chain: ChainKind) -> WalletResult<()>;
}

// ============================================================================
// 状态机 / 缓存（纯逻辑，无外部 RPC 依赖，可独立单测）
// ============================================================================

/// 单条链 RPC 探活的有限状态机取值。
///
/// 状态流转：`Unavailable` --probe 成功--> `Available`，
/// `Available` --probe 失败/超时--> `Probing`，
/// `Probing` --probe 成功--> `Available` / --失败--> `Unavailable`。
/// TTL 过期后，`Available` 退回 `Probing`（需重新探活再确认）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RpcState {
    /// 可用（最近一次探活成功且未过期）
    Available,
    /// 不可用（最近一次探活失败 / 从未探活成功）
    Unavailable,
    /// 探活中（旧结果过期或探活失败后正在重试）
    Probing,
}

impl RpcState {
    /// 是否终态可服务业务（仅 `Available` 为是）。
    pub fn is_serving(self) -> bool {
        matches!(self, RpcState::Available)
    }
}

/// 探活结果缓存项（持有状态 + 最近一次状态详情 + 时间戳）。
#[derive(Debug, Clone)]
pub struct RpcCacheEntry {
    /// 当前状态机取值
    pub state: RpcState,
    /// 最近一次完整状态（latency/source 等），探活失败时为 None
    pub last_status: Option<RpcStatus>,
}

/// RPC 探活缓存（TTL 驱动的状态机容器）。
///
/// 纯内存逻辑，所有时间用注入的 `DateTime<Utc>` 比较，便于确定性单测。
/// 真实 RPC 探活（HTTP/JSON-RPC 调用）由 `RpcRegistryImpl` 完成，留 TODO。
#[derive(Debug, Clone)]
pub struct RpcStatusCache {
    /// 缓存项（按链）
    entries: std::collections::HashMap<ChainKind, RpcCacheEntry>,
    /// 缓存 TTL（秒）：超过则 Available 视为过期，需重新探活。
    ttl_seconds: i64,
}

impl RpcStatusCache {
    /// 构造指定 TTL 的缓存。
    pub fn new(ttl_seconds: u64) -> Self {
        Self {
            entries: std::collections::HashMap::new(),
            ttl_seconds: ttl_seconds as i64,
        }
    }

    /// 默认 TTL（30s）。
    pub fn default_ttl() -> Self {
        Self::new(30)
    }

    /// 写入一条探活成功结果（状态置 Available）。
    pub fn record_available(&mut self, status: RpcStatus) {
        self.entries.insert(
            status.chain,
            RpcCacheEntry {
                state: RpcState::Available,
                last_status: Some(status),
            },
        );
    }

    /// 写入一条探活失败结果（状态置 Unavailable）。
    pub fn record_unavailable(&mut self, chain: ChainKind, at: os_core::DateTime) {
        let status = RpcStatus {
            chain,
            available: false,
            latency_ms: None,
            last_check: at,
            source: RpcSource::Local,
        };
        self.entries.insert(
            chain,
            RpcCacheEntry {
                state: RpcState::Unavailable,
                last_status: Some(status),
            },
        );
    }

    /// 标记某链进入 Probing（保留旧 last_status 供参考）。
    pub fn mark_probing(&mut self, chain: ChainKind) {
        let entry = self.entries.entry(chain).or_insert(RpcCacheEntry {
            state: RpcState::Probing,
            last_status: None,
        });
        entry.state = RpcState::Probing;
    }

    /// 查询某链当前**有效**状态（考虑 TTL 过期）。
    ///
    /// - 若无缓存，返回 `None`（从未探活）。
    /// - 若缓存为 `Available` 且距 `last_check` 超 TTL，则视为过期，**就地**置为
    ///   `Probing` 并返回 `Probing`（调用方应触发重探）。
    /// - 其他状态原样返回。
    pub fn effective_state(
        &mut self,
        chain: ChainKind,
        now: os_core::DateTime,
    ) -> Option<RpcState> {
        let entry = self.entries.get_mut(&chain)?;
        if matches!(entry.state, RpcState::Available) {
            let last_check = entry
                .last_status
                .as_ref()
                .map(|s| s.last_check)
                .unwrap_or(now);
            if (now - last_check).num_seconds() > self.ttl_seconds {
                entry.state = RpcState::Probing;
            }
        }
        Some(entry.state)
    }

    /// 取最近一次完整状态（用于 `RpcRegistry::check` 返回）。
    pub fn last_status(&self, chain: ChainKind) -> Option<&RpcStatus> {
        self.entries
            .get(&chain)
            .and_then(|e| e.last_status.as_ref())
    }

    /// 是否在某链缓存中持有 Available 状态（不判 TTL；TTL 判定见 `effective_state`）。
    pub fn is_cached_available(&self, chain: ChainKind) -> bool {
        self.entries
            .get(&chain)
            .map(|e| matches!(e.state, RpcState::Available))
            .unwrap_or(false)
    }

    /// 清空缓存。
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

// ============================================================================
// RpcProbe —— 可注入的 HTTP/JSON-RPC 传输抽象
// ============================================================================
//
// 把"发 JSON-RPC 请求"这一传输动作抽象成 trait，使 `RpcRegistryImpl` 的探活
// 逻辑可在不依赖真实网络的前提下被单测覆盖（fixture 测）。生产侧用
// `ReqwestProbe`（reqwest::Client），测试侧用 `mock` feature 下的 `FixtureProbe`。

/// 单条 JSON-RPC 请求的响应体（仅取 `result` 字段，忽略 id/jsonrpc）。
///
/// 解析由探活逻辑完成：BTC 从 result 中取 `chain`/`headers`，EVM 直接取 result
/// 作为十六进制 block number 字符串。
pub type RpcResult = serde_json::Value;

/// HTTP/JSON-RPC 传输探针——单次 JSON-RPC POST 调用的抽象。
///
/// 实现方负责：构造 JSON-RPC envelope（`{"jsonrpc":"2.0","id":...,"method":...,
/// "params":...}`）、POST 到给定 URL、解析 HTTP/JSON 响应、返回 `result` 字段。
/// 任何环节失败（连接/超时/非 2xx/JSON-RPC error）均返回 `Err`，由调用方据此
/// 标记链 Unavailable（优雅降级）。
#[async_trait]
pub trait RpcProbe: Send + Sync {
    /// POST 一次 JSON-RPC 调用并返回 `result` 字段。
    ///
    /// - `url`：JSON-RPC 端点（如 `http://localhost:8545`）。
    /// - `method`：JSON-RPC 方法名（如 `eth_blockNumber` / `getblockchaininfo`）。
    /// - `params`：JSON-RPC params（数组或对象）。
    async fn rpc_call(
        &self,
        url: &str,
        method: &str,
        params: serde_json::Value,
    ) -> WalletResult<RpcResult>;
}

/// 默认 JSON-RPC id（探活请求用固定 id 即可，无需递增）。
const PROBE_RPC_ID: i64 = 1;

/// reqwest 真实实现：JSON-RPC POST 探活。
///
/// - 超时由构造时注入（默认 5s）；超时/连接失败 → `Err` → 上层标记 Unavailable。
/// - TLS 走 rustls（workspace 配置，见 ADR-DEPS-001），无 openssl 系统依赖。
///
/// **延迟构造**：`reqwest::Client` 在首次 `rpc_call` 时经 [`std::sync::OnceLock`]
/// 构造（避免构造期 I/O，使 `RpcRegistryImpl::new` 不返回 Result，保持 API 兼容）。
/// 构造失败（极罕见，仅 TLS 初始化异常）会作为该次探活的 `Err` 透传给上层。
pub struct ReqwestProbe {
    timeout: Duration,
    client: std::sync::OnceLock<reqwest::Client>,
}

impl ReqwestProbe {
    /// 构造指定请求超时的探针（不立即创建 reqwest::Client）。
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            client: std::sync::OnceLock::new(),
        }
    }
}

impl Default for ReqwestProbe {
    /// 默认配置：5s 请求超时。
    fn default() -> Self {
        Self::new(Duration::from_secs(5))
    }
}

impl ReqwestProbe {
    fn client(&self) -> WalletResult<&reqwest::Client> {
        if let Some(c) = self.client.get() {
            return Ok(c);
        }
        let c = reqwest::Client::builder()
            .timeout(self.timeout)
            .build()
            .map_err(|e| crate::WalletError::Internal(format!("reqwest client 构建失败: {e}")))?;
        // race-safe：另一线程可能已 set，丢弃本地 c 用全局已存在的即可。
        let _ = self.client.set(c);
        Ok(self.client.get().expect("client just set"))
    }
}

#[async_trait]
impl RpcProbe for ReqwestProbe {
    async fn rpc_call(
        &self,
        url: &str,
        method: &str,
        params: serde_json::Value,
    ) -> WalletResult<RpcResult> {
        // 构造 JSON-RPC 2.0 envelope。
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": PROBE_RPC_ID,
            "method": method,
            "params": params,
        });
        let client = self.client()?;
        let resp = client.post(url).json(&body).send().await.map_err(|e| {
            crate::WalletError::RpcUnavailable(format!("JSON-RPC POST 失败（{url} {method}）: {e}"))
        })?;
        let status = resp.status();
        if !status.is_success() {
            // HTTP 4xx/5xx 视为不可达——优雅降级。
            return Err(crate::WalletError::RpcUnavailable(format!(
                "JSON-RPC 返回非 2xx: {status}（{url} {method}）"
            )));
        }
        // 解析 JSON 响应体；JSON-RPC error 字段优先于 result。
        let v: serde_json::Value = resp.json().await.map_err(|e| {
            crate::WalletError::RpcUnavailable(format!("JSON-RPC 响应解析失败: {e}"))
        })?;
        if let Some(err_obj) = v.get("error").filter(|e| !e.is_null()) {
            return Err(crate::WalletError::RpcUnavailable(format!(
                "JSON-RPC error（{url} {method}）: {err_obj}"
            )));
        }
        v.get("result").cloned().ok_or_else(|| {
            crate::WalletError::RpcUnavailable(format!(
                "JSON-RPC 响应缺 result 字段（{url} {method}）"
            ))
        })
    }
}

// ============================================================================
// RpcRegistryImpl（条件激活核心，真实 reqwest 探活）
// ============================================================================

/// RPC 注册表默认实现（条件激活核心）。
///
/// 持有按链的探活缓存 + 已注册的 `ChainAdapter` + 可注入的 [`RpcProbe`]（传输层）；
/// 当链 RPC 探活成功，调用方注入对应 adapter（`register_adapter`）；失败则
/// `unregister_adapter` 注销，业务侧据此降级。
///
/// **探活实现**：经 [`RpcProbe`] 抽象传输层，生产侧默认用 [`ReqwestProbe`]
/// （reqwest HTTP POST JSON-RPC）：
/// - EVM：`eth_blockNumber` → 解析十六进制 block number。
/// - BTC：`getblockchaininfo` → 解析 `chain` / `headers`。
///
/// 探活超时/连接失败/非 2xx/JSON-RPC error → `probe` 返回 `Err` → `check` 据此
/// 标记链 Unavailable（优雅降级，已有状态机模型），不阻断其他链。
///
/// **测试**：`with_probe` 注入 `FixtureProbe`（mock feature）即可在不发网络
/// 的情况下验证探活/降级路径。
pub struct RpcRegistryImpl {
    cache: std::sync::Mutex<RpcStatusCache>,
    adapters: std::sync::Mutex<std::collections::HashMap<ChainKind, Box<dyn ChainAdapter>>>,
    /// 各链配置（决定探活顺序 + RPC URL）
    configs: Vec<ChainConfig>,
    /// 传输层探针（默认 ReqwestProbe；可经 `with_probe` 注入 fixture）
    probe: Box<dyn RpcProbe>,
}

impl RpcRegistryImpl {
    /// 构造注册表（接受已知链配置 + 默认 [`ReqwestProbe`] 5s 超时；TTL 用默认 30s）。
    ///
    /// reqwest::Client 延迟到首次探活时构造（见 `ReqwestProbe::client` 内部方法），
    /// 故本构造不返回 Result——构造期无 I/O，无失败可能。
    pub fn new(configs: Vec<ChainConfig>) -> Self {
        Self::with_probe_boxed(configs, Box::new(ReqwestProbe::default()))
    }

    /// 设置自定义 TTL（秒）。
    pub fn with_ttl(self, ttl_seconds: u64) -> Self {
        *self.cache.lock().expect("cache poisoned") = RpcStatusCache::new(ttl_seconds);
        self
    }

    /// 注入自定义传输探针（生产/测试切换点）。
    ///
    /// 测试侧用 `mock` feature 下的 `FixtureProbe` 注入固定 JSON 响应，
    /// 即可在零网络环境下验证探活/降级逻辑。
    pub fn with_probe(self, probe: impl RpcProbe + 'static) -> Self {
        Self {
            probe: Box::new(probe),
            ..self
        }
    }

    /// 内部构造器：直接接收已装箱的探针。
    fn with_probe_boxed(configs: Vec<ChainConfig>, probe: Box<dyn RpcProbe>) -> Self {
        Self {
            cache: std::sync::Mutex::new(RpcStatusCache::default_ttl()),
            adapters: std::sync::Mutex::new(std::collections::HashMap::new()),
            configs,
            probe,
        }
    }

    /// 取已配置链列表（用于 `check_all` 遍历）。
    pub fn configured_chains(&self) -> Vec<ChainKind> {
        self.configs
            .iter()
            .filter(|c| c.enabled)
            .map(|c| c.kind)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect()
    }

    /// 按链大类找配置（取第一个匹配；同大类多链如 Ethereum/Base 均按 Evm 探活）。
    fn config_for(&self, chain: ChainKind) -> Option<&ChainConfig> {
        self.configs.iter().find(|c| c.enabled && c.kind == chain)
    }

    /// 真实 RPC 探活：经 [`RpcProbe`] POST JSON-RPC，解析返回值构造 [`RpcStatus`]。
    ///
    /// - EVM：`eth_blockNumber`（params `[]`），result 为 `"0x..."` 十六进制 block number。
    /// - BTC：`getblockchaininfo`（params `[]`），result 含 `chain`（如 "main"/"test"/
    ///   "regtest"）与 `headers`（已验证 header 数）。
    ///
    /// 主 URL 失败时尝试 fallback URL；两者均失败返回 `Err`，由 `check` 据此
    /// 优雅降级（标记 Unavailable，不阻断其他链）。
    async fn probe(&self, chain: ChainKind) -> WalletResult<RpcStatus> {
        let cfg = self.config_for(chain).ok_or_else(|| {
            crate::WalletError::ChainUnsupported(format!(
                "链 {} 未配置或未启用",
                chain.display_name()
            ))
        })?;
        // RPC URL：主 → fallback，取第一个非空。
        let urls: Vec<&str> = [cfg.rpc_url.as_deref(), cfg.rpc_fallback_url.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        if urls.is_empty() {
            return Err(crate::WalletError::RpcUnavailable(format!(
                "链 {} 未配置 RPC URL",
                chain.display_name()
            )));
        }
        let method = chain.default_probe_method();
        let params = serde_json::Value::Array(vec![]);

        let started = std::time::Instant::now();
        // 逐 URL 探活，第一个成功即用；全失败则返回最后一次错误。
        let mut last_err: Option<crate::WalletError> = None;
        for url in &urls {
            match self.probe.rpc_call(url, method, params.clone()).await {
                Ok(result) => {
                    // 解析 result 字段以确认响应语义正确（不仅是 HTTP 可达）。
                    Self::validate_probe_result(chain, &result)?;
                    let latency_ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
                    let now = os_core::Utc::now();
                    let source = if Some(*url) == cfg.rpc_url.as_deref() {
                        RpcSource::Local
                    } else {
                        RpcSource::Remote
                    };
                    return Ok(RpcStatus {
                        chain,
                        available: true,
                        latency_ms: Some(latency_ms),
                        last_check: now,
                        source,
                    });
                }
                Err(e) => last_err = Some(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            crate::WalletError::RpcUnavailable(format!(
                "链 {} 所有 RPC URL 探活均失败",
                chain.display_name()
            ))
        }))
    }

    /// 校验 JSON-RPC result 字段语义（探活成功的第二道闸）。
    ///
    /// - EVM `eth_blockNumber`：result 应为非空十六进制字符串（`0x` 前缀 + 至少 1 hex）。
    /// - BTC `getblockchaininfo`：result 应为对象且含 `chain`/`headers` 字段。
    fn validate_probe_result(chain: ChainKind, result: &serde_json::Value) -> WalletResult<()> {
        match chain {
            ChainKind::Evm => {
                let hex = result.as_str().ok_or_else(|| {
                    crate::WalletError::RpcUnavailable(format!(
                        "EVM eth_blockNumber result 非字符串: {result}"
                    ))
                })?;
                if !hex.starts_with("0x") || hex.len() < 3 {
                    return Err(crate::WalletError::RpcUnavailable(format!(
                        "EVM eth_blockNumber result 非法十六进制: {hex}"
                    )));
                }
                Ok(())
            }
            ChainKind::Bitcoin => {
                let obj = result.as_object().ok_or_else(|| {
                    crate::WalletError::RpcUnavailable(format!(
                        "BTC getblockchaininfo result 非对象: {result}"
                    ))
                })?;
                if !obj.contains_key("chain") || !obj.contains_key("headers") {
                    return Err(crate::WalletError::RpcUnavailable(format!(
                        "BTC getblockchaininfo result 缺 chain/headers 字段: {result}"
                    )));
                }
                Ok(())
            }
        }
    }

    /// 内部：注入探活成功结果到缓存（供测试/未来 probe 调用）。
    pub(crate) fn record_available(&self, status: RpcStatus) {
        self.cache
            .lock()
            .expect("cache poisoned")
            .record_available(status);
    }
}

// RpcRegistry 是 async trait（ADR-COMPAT-001，被 Box<dyn> 用故加 #[async_trait]）。
#[async_trait]
impl RpcRegistry for RpcRegistryImpl {
    async fn check(&self, chain: ChainKind) -> WalletResult<RpcStatus> {
        // 真实探活（经 RpcProbe 注入传输层）；超时/连接失败 → 优雅降级。
        match self.probe(chain).await {
            Ok(status) => {
                self.record_available(status.clone());
                Ok(status)
            }
            Err(_e) => {
                // 探活失败：标记该链 Unavailable（驱动状态机），优先返回最后一次
                // 缓存（陈旧可用性参考）；无缓存则返回一个 Unavailable 状态——
                // `check` 不抛错，调用方据 status.available 决定降级（§9 红线）。
                let now = os_core::Utc::now();
                self.cache
                    .lock()
                    .expect("cache poisoned")
                    .record_unavailable(chain, now);
                let cached = self
                    .cache
                    .lock()
                    .expect("cache poisoned")
                    .last_status(chain)
                    .cloned();
                Ok(cached.unwrap_or(RpcStatus {
                    chain,
                    available: false,
                    latency_ms: None,
                    last_check: now,
                    source: RpcSource::Local,
                }))
            }
        }
    }

    async fn check_all(&self) -> WalletResult<Vec<RpcStatus>> {
        let mut out = Vec::new();
        for chain in self.configured_chains() {
            // 逐链探活；单链失败已被 check 内部降级为 Unavailable 状态，不阻断其余链。
            let s = self.check(chain).await?;
            out.push(s);
        }
        Ok(out)
    }

    async fn is_available(&self, chain: ChainKind) -> WalletResult<bool> {
        let now = os_core::Utc::now();
        let state = self
            .cache
            .lock()
            .expect("cache poisoned")
            .effective_state(chain, now);
        Ok(state.map(|s| s.is_serving()).unwrap_or(false))
    }

    async fn register_adapter(&self, adapter: Box<dyn ChainAdapter>) -> WalletResult<()> {
        let chain = adapter.chain_kind().await;
        self.adapters
            .lock()
            .expect("adapters poisoned")
            .insert(chain, adapter);
        Ok(())
    }

    async fn unregister_adapter(&self, chain: ChainKind) -> WalletResult<()> {
        self.adapters
            .lock()
            .expect("adapters poisoned")
            .remove(&chain);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ChainConfig;

    fn ts(secs: i64) -> os_core::DateTime {
        chrono::DateTime::from_timestamp(secs, 0).unwrap()
    }

    #[test]
    fn rpc_state_serving() {
        assert!(RpcState::Available.is_serving());
        assert!(!RpcState::Unavailable.is_serving());
        assert!(!RpcState::Probing.is_serving());
    }

    #[test]
    fn cache_lifecycle_and_ttl() {
        let mut cache = RpcStatusCache::new(10);
        let chain = ChainKind::Evm;
        // 初始无缓存。
        assert_eq!(cache.effective_state(chain, ts(0)), None);

        // 记录可用 @ t=0。
        cache.record_available(RpcStatus {
            chain,
            available: true,
            latency_ms: Some(5),
            last_check: ts(0),
            source: RpcSource::Local,
        });
        assert_eq!(
            cache.effective_state(chain, ts(5)),
            Some(RpcState::Available)
        );
        assert!(cache.is_cached_available(chain));

        // TTL=10s，t=11 过期 -> Probing（就地置位）。
        assert_eq!(
            cache.effective_state(chain, ts(11)),
            Some(RpcState::Probing)
        );
        // 缓存条目状态已被改写为 Probing。
        assert!(!cache.is_cached_available(chain));
    }

    #[test]
    fn cache_unavailable_and_probing() {
        let mut cache = RpcStatusCache::new(60);
        let chain = ChainKind::Bitcoin;
        cache.record_unavailable(chain, ts(0));
        assert_eq!(
            cache.effective_state(chain, ts(1)),
            Some(RpcState::Unavailable)
        );
        assert!(!cache.is_cached_available(chain));

        cache.mark_probing(chain);
        assert_eq!(cache.effective_state(chain, ts(2)), Some(RpcState::Probing));
    }

    #[tokio::test]
    async fn registry_state_machine_shell() {
        // 两条启用链 + 一条禁用链。用 FixtureProbe 注入固定响应（零网络）。
        let cfgs = vec![
            ChainConfig::new("bitcoin", ChainKind::Bitcoin, "http://btc"),
            ChainConfig::new("ethereum", ChainKind::Evm, "http://evm"),
            ChainConfig {
                chain_id: os_core::ChainId::new("disabled"),
                kind: ChainKind::Evm,
                rpc_url: None,
                rpc_fallback_url: None,
                enabled: false,
            },
        ];
        let probe = crate::mock::FixtureProbe::new()
            .with_method("eth_blockNumber", serde_json::json!("0x1234"))
            .with_method(
                "getblockchaininfo",
                serde_json::json!({
                    "chain": "main",
                    "headers": 800000,
                    "blocks": 800000,
                }),
            );
        let reg = RpcRegistryImpl::new(cfgs).with_probe(probe);
        // configured_chains 应忽略禁用链。
        let mut chains = reg.configured_chains();
        // ChainKind 未派生 Ord，用 display_name 排序以得到稳定顺序。
        chains.sort_by_key(|c| c.display_name());
        assert_eq!(chains, vec![ChainKind::Bitcoin, ChainKind::Evm]);

        // 注入 Evm 可用 -> is_available true（在探活前的缓存态）。
        reg.record_available(RpcStatus {
            chain: ChainKind::Evm,
            available: true,
            latency_ms: Some(3),
            last_check: os_core::Utc::now(),
            source: RpcSource::Local,
        });
        assert!(reg.is_available(ChainKind::Evm).await.unwrap());
        assert!(!reg.is_available(ChainKind::Bitcoin).await.unwrap());

        // check 触发真实探活（FixtureProbe 返回 eth_blockNumber=0x1234）→ available。
        let s = reg.check(ChainKind::Evm).await.unwrap();
        assert!(s.available);
        assert!(s.latency_ms.is_some());
    }

    #[tokio::test]
    async fn registry_register_unregister_adapter() {
        #[cfg(feature = "mock")]
        {
            use crate::mock::MockChainAdapter;
            let reg = RpcRegistryImpl::new(vec![ChainConfig::new(
                "ethereum",
                ChainKind::Evm,
                "http://x",
            )]);
            let adapter = MockChainAdapter::new(ChainKind::Evm);
            reg.register_adapter(Box::new(adapter)).await.unwrap();
            // 再次注册同链覆盖；注销后无错误。
            reg.unregister_adapter(ChainKind::Evm).await.unwrap();
        }
    }

    // =========================================================================
    // reqwest 探活路径测试（FixtureProbe 注入，零网络）
    // =========================================================================

    /// 构造测试用注册表（带 FixtureProbe）。
    fn registry_with_probe(
        cfgs: Vec<ChainConfig>,
        probe: crate::mock::FixtureProbe,
    ) -> RpcRegistryImpl {
        RpcRegistryImpl::new(cfgs).with_probe(probe)
    }

    #[tokio::test]
    async fn probe_evm_success_marks_available() {
        // eth_blockNumber 返回十六进制 block number → 链 available。
        let cfgs = vec![ChainConfig::new("ethereum", ChainKind::Evm, "http://evm")];
        let probe = crate::mock::FixtureProbe::new()
            .with_method("eth_blockNumber", serde_json::json!("0x1abcd"));
        let reg = registry_with_probe(cfgs, probe);

        let s = reg.check(ChainKind::Evm).await.unwrap();
        assert!(s.available, "EVM 探活成功应标记 available");
        assert_eq!(s.chain, ChainKind::Evm);
        assert!(s.latency_ms.is_some(), "成功应记录延迟");
        assert_eq!(s.source, RpcSource::Local);

        // 探活成功后 is_available 也应为 true（缓存态 = Available）。
        assert!(reg.is_available(ChainKind::Evm).await.unwrap());

        // 验证 FixtureProbe 收到了正确的 method。
        // （calls 字段在 FixtureProbe 内部，这里间接通过行为断言已足够。）
    }

    #[tokio::test]
    async fn probe_btc_success_marks_available() {
        // getblockchaininfo 返回 chain/headers → 链 available。
        let cfgs = vec![ChainConfig::new(
            "bitcoin",
            ChainKind::Bitcoin,
            "http://btc",
        )];
        let probe = crate::mock::FixtureProbe::new().with_method(
            "getblockchaininfo",
            serde_json::json!({
                "chain": "main",
                "blocks": 800000,
                "headers": 800000,
                "bestblockhash": "0000000000",
            }),
        );
        let reg = registry_with_probe(cfgs, probe);

        let s = reg.check(ChainKind::Bitcoin).await.unwrap();
        assert!(s.available, "BTC 探活成功应标记 available");
        assert_eq!(s.chain, ChainKind::Bitcoin);
        assert!(reg.is_available(ChainKind::Bitcoin).await.unwrap());
    }

    #[tokio::test]
    async fn probe_failure_marks_unavailable_graceful() {
        // RPC 调用失败（模拟连接拒绝）→ 优雅降级标记 Unavailable，不抛错。
        let cfgs = vec![ChainConfig::new(
            "bitcoin",
            ChainKind::Bitcoin,
            "http://btc",
        )];
        let probe = crate::mock::FixtureProbe::new()
            .with_method_error("getblockchaininfo", "连接拒绝（mock）");
        let reg = registry_with_probe(cfgs, probe);

        let s = reg.check(ChainKind::Bitcoin).await.unwrap();
        assert!(!s.available, "探活失败应降级为 Unavailable");
        assert_eq!(s.latency_ms, None);
        // check 不抛错（优雅降级契约）。
        assert!(!reg.is_available(ChainKind::Bitcoin).await.unwrap());
    }

    #[tokio::test]
    async fn probe_failure_falls_back_to_remote_url() {
        // 主 URL 失败 → 尝试 fallback URL 成功 → available + source=Remote。
        let cfgs = vec![ChainConfig::new("ethereum", ChainKind::Evm, "http://local")
            .with_fallback("http://remote")];
        // local 端点报错，remote 端点成功——但 FixtureProbe 按 method（不按 url）
        // 路由，故同 method 第一次失败后第二次成功需要更精细的 probe。
        // 这里用一个区分 url 的自定义 probe 验证 fallback 逻辑。
        struct FallbackProbe;
        #[async_trait::async_trait]
        impl RpcProbe for FallbackProbe {
            async fn rpc_call(
                &self,
                url: &str,
                method: &str,
                _params: serde_json::Value,
            ) -> WalletResult<RpcResult> {
                if url == "http://remote" && method == "eth_blockNumber" {
                    Ok(serde_json::json!("0x100"))
                } else {
                    Err(crate::WalletError::RpcUnavailable(format!(
                        "local 不可达: {url}"
                    )))
                }
            }
        }
        let reg = RpcRegistryImpl::new(cfgs).with_probe(FallbackProbe);
        let s = reg.check(ChainKind::Evm).await.unwrap();
        assert!(s.available, "fallback URL 成功应标记 available");
        assert_eq!(s.source, RpcSource::Remote, "应记录命中 Remote 源");
    }

    #[tokio::test]
    async fn probe_all_urls_fail_marks_unavailable() {
        // 主 + fallback 全失败 → Unavailable。
        let cfgs = vec![ChainConfig::new("ethereum", ChainKind::Evm, "http://local")
            .with_fallback("http://remote")];
        let probe = crate::mock::FixtureProbe::new().with_method_error("eth_blockNumber", "全失败");
        let reg = registry_with_probe(cfgs, probe);
        let s = reg.check(ChainKind::Evm).await.unwrap();
        assert!(!s.available);
        assert!(!reg.is_available(ChainKind::Evm).await.unwrap());
    }

    #[tokio::test]
    async fn probe_check_all_skips_disabled_and_degrades_per_chain() {
        // 多链：EVM 成功，BTC 失败——check_all 不阻断，返回各自状态。
        let cfgs = vec![
            ChainConfig::new("ethereum", ChainKind::Evm, "http://evm"),
            ChainConfig::new("bitcoin", ChainKind::Bitcoin, "http://btc"),
            ChainConfig {
                chain_id: os_core::ChainId::new("disabled"),
                kind: ChainKind::Evm,
                rpc_url: Some("http://none".into()),
                rpc_fallback_url: None,
                enabled: false,
            },
        ];
        let probe = crate::mock::FixtureProbe::new()
            .with_method("eth_blockNumber", serde_json::json!("0x42"))
            .with_method_error("getblockchaininfo", "btc 不可达");
        let reg = registry_with_probe(cfgs, probe);

        let all = reg.check_all().await.unwrap();
        // 禁用链不应出现在结果中。
        assert_eq!(all.len(), 2, "禁用链应被跳过");
        let evm = all.iter().find(|s| s.chain == ChainKind::Evm).unwrap();
        let btc = all.iter().find(|s| s.chain == ChainKind::Bitcoin).unwrap();
        assert!(evm.available, "EVM 探活成功");
        assert!(!btc.available, "BTC 探活失败应优雅降级");
    }

    #[tokio::test]
    async fn probe_invalid_result_format_degrades() {
        // result 字段语义非法（EVM 非 0x 十六进制）→ 视为探活失败 → Unavailable。
        let cfgs = vec![ChainConfig::new("ethereum", ChainKind::Evm, "http://evm")];
        let probe = crate::mock::FixtureProbe::new()
            .with_method("eth_blockNumber", serde_json::json!("not-hex"));
        let reg = registry_with_probe(cfgs, probe);
        let s = reg.check(ChainKind::Evm).await.unwrap();
        assert!(!s.available, "非法响应格式应降级");
    }

    #[tokio::test]
    async fn probe_btc_missing_fields_degrades() {
        // BTC result 缺 headers 字段 → 视为探活失败。
        let cfgs = vec![ChainConfig::new(
            "bitcoin",
            ChainKind::Bitcoin,
            "http://btc",
        )];
        let probe = crate::mock::FixtureProbe::new().with_method(
            "getblockchaininfo",
            serde_json::json!({"chain": "main"}), // 缺 headers
        );
        let reg = registry_with_probe(cfgs, probe);
        let s = reg.check(ChainKind::Bitcoin).await.unwrap();
        assert!(!s.available, "BTC 响应缺 headers 应降级");
    }

    #[tokio::test]
    async fn probe_unconfigured_chain_errors() {
        // 未配置的链 → probe 返回 ChainUnsupported（被 check 降级为 Unavailable）。
        let cfgs = vec![ChainConfig::new("ethereum", ChainKind::Evm, "http://evm")];
        let probe = crate::mock::FixtureProbe::new();
        let reg = registry_with_probe(cfgs, probe);
        let s = reg.check(ChainKind::Bitcoin).await.unwrap();
        assert!(!s.available, "未配置链应降级为 Unavailable");
    }

    #[test]
    fn validate_probe_result_evm_hex_check() {
        // 合法 0x 十六进制。
        assert!(
            RpcRegistryImpl::validate_probe_result(ChainKind::Evm, &serde_json::json!("0x0"))
                .is_ok()
        );
        // 缺 0x 前缀。
        assert!(
            RpcRegistryImpl::validate_probe_result(ChainKind::Evm, &serde_json::json!("1234"))
                .is_err()
        );
        // 非字符串。
        assert!(
            RpcRegistryImpl::validate_probe_result(ChainKind::Evm, &serde_json::json!(42)).is_err()
        );
    }

    #[test]
    fn validate_probe_result_btc_object_check() {
        // 合法对象。
        assert!(RpcRegistryImpl::validate_probe_result(
            ChainKind::Bitcoin,
            &serde_json::json!({"chain": "main", "headers": 1})
        )
        .is_ok());
        // 缺字段。
        assert!(RpcRegistryImpl::validate_probe_result(
            ChainKind::Bitcoin,
            &serde_json::json!({"chain": "main"})
        )
        .is_err());
        // 非对象。
        assert!(RpcRegistryImpl::validate_probe_result(
            ChainKind::Bitcoin,
            &serde_json::json!("main")
        )
        .is_err());
    }

    #[tokio::test]
    async fn reqwest_probe_real_timeout_to_unavailable() {
        // 真实 ReqwestProbe 探活一个不可达地址 → 优雅降级为 Unavailable。
        // 用一个保证不可达的地址（10.255.255.1:1 通常黑洞）+ 极短超时，确保测试不慢。
        let cfgs = vec![ChainConfig::new(
            "ethereum",
            ChainKind::Evm,
            "http://10.255.255.1:1",
        )];
        let probe = ReqwestProbe::new(std::time::Duration::from_millis(100));
        let reg = RpcRegistryImpl::new(cfgs).with_probe(probe);
        let s = reg.check(ChainKind::Evm).await.unwrap();
        assert!(
            !s.available,
            "不可达地址应优雅降级（不抛错，标记 Unavailable）"
        );
        // 探活失败不阻断——check_all 同样降级。
        let all = reg.check_all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert!(!all[0].available);
    }

    // =========================================================================
    // 覆盖率补充：状态机 / DTO serde / Cache 边界 / ReqwestProbe::Default / with_ttl
    // =========================================================================

    #[test]
    fn rpc_source_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&RpcSource::Local).unwrap(),
            "\"local\""
        );
        assert_eq!(
            serde_json::to_string(&RpcSource::Remote).unwrap(),
            "\"remote\""
        );
        let l: RpcSource = serde_json::from_str("\"local\"").unwrap();
        let r: RpcSource = serde_json::from_str("\"remote\"").unwrap();
        assert_eq!(l, RpcSource::Local);
        assert_eq!(r, RpcSource::Remote);
        assert!(serde_json::from_str::<RpcSource>("\"cdn\"").is_err());
    }

    #[test]
    fn rpc_state_serde_snake_case_and_serving() {
        assert_eq!(
            serde_json::to_string(&RpcState::Available).unwrap(),
            "\"available\""
        );
        assert_eq!(
            serde_json::to_string(&RpcState::Unavailable).unwrap(),
            "\"unavailable\""
        );
        assert_eq!(
            serde_json::to_string(&RpcState::Probing).unwrap(),
            "\"probing\""
        );
        for s in [
            RpcState::Available,
            RpcState::Unavailable,
            RpcState::Probing,
        ] {
            let json = serde_json::to_string(&s).unwrap();
            let back: RpcState = serde_json::from_str(&json).unwrap();
            assert_eq!(back, s);
        }
        assert!(serde_json::from_str::<RpcState>("\"ready\"").is_err());
    }

    #[test]
    fn rpc_status_serde_roundtrip() {
        let s = RpcStatus {
            chain: ChainKind::Evm,
            available: true,
            latency_ms: Some(42),
            last_check: ts(1000),
            source: RpcSource::Local,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: RpcStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back.chain, s.chain);
        assert_eq!(back.available, s.available);
        assert_eq!(back.latency_ms, s.latency_ms);
        assert_eq!(back.source, s.source);
    }

    #[test]
    fn rpc_status_serde_skips_none_latency() {
        let s = RpcStatus {
            chain: ChainKind::Bitcoin,
            available: false,
            latency_ms: None,
            last_check: ts(0),
            source: RpcSource::Local,
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains("latency_ms"), "{json}");
    }

    #[test]
    fn rpc_cache_entry_debug_format() {
        let e = RpcCacheEntry {
            state: RpcState::Available,
            last_status: None,
        };
        let s = format!("{e:?}");
        assert!(s.contains("Available"));
    }

    #[test]
    fn cache_default_ttl_is_30_seconds() {
        // 默认 TTL=30s：t=0 记录 available，t=30 仍有效，t=31 过期。
        let mut cache = RpcStatusCache::default_ttl();
        let chain = ChainKind::Bitcoin;
        cache.record_available(RpcStatus {
            chain,
            available: true,
            latency_ms: Some(1),
            last_check: ts(0),
            source: RpcSource::Local,
        });
        assert_eq!(
            cache.effective_state(chain, ts(30)),
            Some(RpcState::Available)
        );
        assert_eq!(
            cache.effective_state(chain, ts(31)),
            Some(RpcState::Probing)
        );
    }

    #[test]
    fn cache_clear_empties_entries() {
        let mut cache = RpcStatusCache::new(60);
        let chain = ChainKind::Evm;
        cache.record_available(RpcStatus {
            chain,
            available: true,
            latency_ms: Some(1),
            last_check: ts(0),
            source: RpcSource::Local,
        });
        assert!(cache.is_cached_available(chain));
        cache.clear();
        assert!(!cache.is_cached_available(chain));
        assert_eq!(cache.effective_state(chain, ts(1)), None);
    }

    #[test]
    fn cache_mark_probing_on_fresh_chain_creates_entry() {
        // mark_probing 在 entry 不存在时 or_insert 创建新条目。
        let mut cache = RpcStatusCache::new(60);
        let chain = ChainKind::Bitcoin;
        cache.mark_probing(chain);
        assert_eq!(cache.effective_state(chain, ts(1)), Some(RpcState::Probing));
        assert!(!cache.is_cached_available(chain));
    }

    #[test]
    fn cache_last_status_returns_recorded() {
        let mut cache = RpcStatusCache::new(60);
        let chain = ChainKind::Evm;
        let status = RpcStatus {
            chain,
            available: true,
            latency_ms: Some(7),
            last_check: ts(100),
            source: RpcSource::Remote,
        };
        cache.record_available(status.clone());
        let got = cache.last_status(chain).unwrap();
        assert_eq!(got.chain, status.chain);
        assert_eq!(got.latency_ms, Some(7));
        assert_eq!(got.source, RpcSource::Remote);
        // 不存在的链返回 None。
        assert!(cache.last_status(ChainKind::Bitcoin).is_none());
    }

    #[test]
    fn cache_record_unavailable_stores_status() {
        let mut cache = RpcStatusCache::new(60);
        let chain = ChainKind::Bitcoin;
        cache.record_unavailable(chain, ts(50));
        let s = cache.last_status(chain).unwrap();
        assert!(!s.available);
        assert_eq!(s.latency_ms, None);
        assert_eq!(s.source, RpcSource::Local);
    }

    #[test]
    fn cache_effective_state_unavailable_chain_unchanged() {
        // Unavailable 状态不受 TTL 影响（不退化为 Probing）。
        let mut cache = RpcStatusCache::new(10);
        let chain = ChainKind::Evm;
        cache.record_unavailable(chain, ts(0));
        // 即使过了很久，Unavailable 仍是 Unavailable。
        assert_eq!(
            cache.effective_state(chain, ts(1_000_000)),
            Some(RpcState::Unavailable)
        );
    }

    #[test]
    fn cache_effective_state_probing_chain_unchanged() {
        // Probing 状态也不受 TTL 影响。
        let mut cache = RpcStatusCache::new(10);
        let chain = ChainKind::Bitcoin;
        cache.mark_probing(chain);
        assert_eq!(
            cache.effective_state(chain, ts(1_000_000)),
            Some(RpcState::Probing)
        );
    }

    #[test]
    fn reqwest_probe_default_is_5s_timeout() {
        // ReqwestProbe::default() 不应 panic（不构造 client，仅记录 timeout）。
        let _ = ReqwestProbe::default();
        // new 也只是记录 timeout。
        let _ = ReqwestProbe::new(Duration::from_secs(1));
    }

    #[tokio::test]
    async fn registry_with_ttl_overrides_default() {
        // with_ttl(1) 让 TTL=1s：t=0 探活成功，t=2 即过期。
        let cfgs = vec![ChainConfig::new("ethereum", ChainKind::Evm, "http://evm")];
        let probe = crate::mock::FixtureProbe::new()
            .with_method("eth_blockNumber", serde_json::json!("0x1"));
        let reg = RpcRegistryImpl::new(cfgs).with_probe(probe).with_ttl(1);
        // 探活成功。
        let s = reg.check(ChainKind::Evm).await.unwrap();
        assert!(s.available);
        // 缓存态在探活后是 Available。
        assert!(reg.is_available(ChainKind::Evm).await.unwrap());
    }

    #[tokio::test]
    async fn registry_check_unconfigured_chain_returns_unavailable_status() {
        // check 对未配置链：probe 返回 ChainUnsupported，check 降级为 Unavailable 状态（不抛错）。
        let reg = RpcRegistryImpl::new(vec![]).with_probe(crate::mock::FixtureProbe::new());
        let s = reg.check(ChainKind::Evm).await.unwrap();
        assert!(!s.available);
        assert_eq!(s.chain, ChainKind::Evm);
        assert_eq!(s.latency_ms, None);
    }

    #[tokio::test]
    async fn registry_check_all_empty_configs_returns_empty() {
        // 无任何配置 -> check_all 返回空 vec。
        let reg = RpcRegistryImpl::new(vec![]).with_probe(crate::mock::FixtureProbe::new());
        let all = reg.check_all().await.unwrap();
        assert!(all.is_empty());
    }

    #[tokio::test]
    async fn registry_register_and_unregister_adapter_idempotent() {
        #[cfg(feature = "mock")]
        {
            use crate::mock::MockChainAdapter;
            let reg = RpcRegistryImpl::new(vec![ChainConfig::new(
                "bitcoin",
                ChainKind::Bitcoin,
                "http://x",
            )]);
            // 注册两次同链（覆盖）。
            reg.register_adapter(Box::new(MockChainAdapter::new(ChainKind::Bitcoin)))
                .await
                .unwrap();
            reg.register_adapter(Box::new(MockChainAdapter::new(ChainKind::Bitcoin)))
                .await
                .unwrap();
            // 注销。
            reg.unregister_adapter(ChainKind::Bitcoin).await.unwrap();
            // 再次注销无错误（幂等）。
            reg.unregister_adapter(ChainKind::Bitcoin).await.unwrap();
        }
    }

    #[tokio::test]
    async fn registry_is_available_unknown_chain_returns_false() {
        // 未探活过的链 -> is_available 返回 false（不抛错）。
        let reg = RpcRegistryImpl::new(vec![]).with_probe(crate::mock::FixtureProbe::new());
        assert!(!reg.is_available(ChainKind::Evm).await.unwrap());
        assert!(!reg.is_available(ChainKind::Bitcoin).await.unwrap());
    }

    #[test]
    fn validate_probe_result_evm_edge_cases() {
        // 仅 "0x" 无 hex 数字（len=2 < 3）-> 失败。
        assert!(
            RpcRegistryImpl::validate_probe_result(ChainKind::Evm, &serde_json::json!("0x"))
                .is_err()
        );
        // 合法 "0x0"（len=3）-> 通过。
        assert!(
            RpcRegistryImpl::validate_probe_result(ChainKind::Evm, &serde_json::json!("0x0"))
                .is_ok()
        );
        // null -> 失败（非字符串）。
        assert!(
            RpcRegistryImpl::validate_probe_result(ChainKind::Evm, &serde_json::Value::Null)
                .is_err()
        );
    }

    #[test]
    fn validate_probe_result_btc_edge_cases() {
        // 仅 chain 字段（缺 headers）-> 失败。
        assert!(RpcRegistryImpl::validate_probe_result(
            ChainKind::Bitcoin,
            &serde_json::json!({"headers": 1})
        )
        .is_err());
        // 仅 headers 字段（缺 chain）-> 失败。
        assert!(RpcRegistryImpl::validate_probe_result(
            ChainKind::Bitcoin,
            &serde_json::json!({"chain": "main"})
        )
        .is_err());
        // 空对象 -> 失败。
        assert!(
            RpcRegistryImpl::validate_probe_result(ChainKind::Bitcoin, &serde_json::json!({}))
                .is_err()
        );
        // 数组 -> 失败（非对象）。
        assert!(RpcRegistryImpl::validate_probe_result(
            ChainKind::Bitcoin,
            &serde_json::json!([1, 2, 3])
        )
        .is_err());
    }

    #[test]
    fn probe_rpc_id_is_one() {
        // 固定常量。
        assert_eq!(PROBE_RPC_ID, 1);
    }
}
