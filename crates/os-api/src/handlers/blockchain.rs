//! `BlockchainRouteHandler` —— 区块链管理（RPC 节点 + 区块链浏览器）桌面应用适配器。
//!
//! 定位：把网关 HTTP 请求（`/api/v1/blockchain/*`）翻译为区块链节点 + 浏览器的
//! **编排管理**。支持 4 类链：ethereum（主网/测试网）/ dev（本地开发链）
//! / l2（Optimism/Arbitrum/Base）/ custom（自定义私有链）。
//!
//! # 编排 + 真实 spawn 语义（docker 未安装也不 panic）
//!
//! start/stop 操作**真实 spawn `docker compose up -d` / `docker compose down`**
//! （cwd=data_dir，先把 compose_yaml 落盘到 `data_dir/docker-compose.yml`）。
//! docker 未安装 / 权限不足 / compose 启动失败 → **降级 status=error 不 panic**。
//! 同时构造的 `compose_yaml` 与 `start_cmd` 字段保留（即便 docker 真跑了也存供查看）。
//! start_cmd / 状态探测属可选增强；start/stop 真跑 docker 是核心。
//!
//! # 路由表（20 条，component="blockchain"）
//!
//! | method | path                                       | 动作 |
//! |--------|--------------------------------------------|------|
//! | GET    | `/api/v1/blockchain/nodes`                 | 列节点 |
//! | POST   | `/api/v1/blockchain/nodes`                 | 创建节点（admin）→ 构造 docker-compose + start_cmd |
//! | GET    | `/api/v1/blockchain/nodes/:id`             | 单节点详情（含 compose_yaml）|
//! | POST   | `/api/v1/blockchain/nodes/:id/start`       | 标记启动（admin）|
//! | POST   | `/api/v1/blockchain/nodes/:id/stop`        | 标记停止（admin）|
//! | DELETE | `/api/v1/blockchain/nodes/:id`             | 删节点（admin）|
//! | GET    | `/api/v1/blockchain/explorers`             | 列浏览器 |
//! | POST   | `/api/v1/blockchain/explorers`             | 创建浏览器（admin）→ 关联 node_id，构造 Blockscout compose |
//! | DELETE | `/api/v1/blockchain/explorers/:id`         | 删浏览器（admin）|
//! | POST   | `/api/v1/blockchain/explorers/:id/start`   | 标记启动（admin）|
//! | GET    | `/api/v1/blockchain/chain-presets`         | 4 类链的预设配置 |
//! | GET    | `/api/v1/blockchain/stats`                 | 聚合统计 |
//! | GET    | `/api/v1/blockchain/clients`               | 支持的客户端列表 + 说明 |
//! | GET    | `/api/v1/blockchain/wallets`               | 列钱包（绝不返回私钥本体）|
//! | POST   | `/api/v1/blockchain/wallets`               | 创建钱包（admin）→ k256+tiny-keccak 纯 Rust 生成密钥对 |
//! | DELETE | `/api/v1/blockchain/wallets/:id`           | 删钱包（admin）|
//! | GET    | `/api/v1/blockchain/wallets/:id/balance`   | 查余额（reqwest eth_getBalance，节点离线降级 null）|
//! | POST   | `/api/v1/blockchain/wallets/:id/import`    | 导入私钥（admin）→ k256 纯 Rust 派生地址 |
//! | POST   | `/api/v1/blockchain/wallets/:id/sign`      | 签名交易（admin）→ spawn python3 eth-account（缺库降级未签名）|
//! | GET    | `/api/v1/blockchain/wallets/:id/address`   | 导出地址（含脱敏格式）|
//!
//! 另有子模块 [`blockchain_nodes`] 的 9 条 `/api/v1/blockchain/chain-nodes*`
//! 路由（geth/bitcoind 真实子进程节点运行管理）与本 handler 同组件注册、
//! handle 侧整体委托（见该模块头路由表与 docs/BLOCKCHAIN_NODES.md）。
//!
//! # 钱包安全红线
//!
//! **私钥绝不明文返回 API**：API 视图 `Wallet` 只含 `has_private_key` 布尔 + 地址；
//! 私钥仅存于本地 `/tank/os-data/wallets.json`（含 `private_key` 字段的落盘结构
//! `WalletRecord`），任何响应序列化前都先经 `wallet_view()` 投影剥离。

use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};

use super::blockchain_nodes::{self, ChainNodeState};
use crate::error::ApiGatewayError;
use crate::gateway::{ApiRequest, ApiResponse, HttpMethod, RouteHandler, RouteSpec};

/// 进程级共享 `reqwest::Client`（rustify：eth_getBalance 的 curl 子进程 → reqwest）。
static HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("构建共享 reqwest Client 失败")
});

// ----------------------------------------------------------------------------
// DTO
// ----------------------------------------------------------------------------

/// 链配置（链类型 + chain_id + 网络 + 显示名）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainConfig {
    /// `ethereum` / `dev` / `l2` / `custom`。
    pub chain_type: String,
    /// 1=主网, 11155111=Sepolia, 1337=本地, 10=Optimism, 8453=Base, 自定义。
    pub chain_id: u64,
    /// `mainnet` / `sepolia` / `goerli` / `dev` / `optimism` / `arbitrum` / `base` / `custom`。
    pub network: String,
    /// 显示名，如 "Ethereum 主网"。
    pub name: String,
}

/// RPC 节点实例（一个 docker 容器 = 一个节点）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInstance {
    pub id: String,
    /// 用户起名，如 "ETH主网节点" / "本地开发链"。
    pub name: String,
    pub chain: ChainConfig,
    /// `geth` / `reth` / `erigon` / `ganache` / `anvil` / `op-geth` / `arbitrum-node`。
    pub client: String,
    /// JSON-RPC 端口，默认 8545。
    pub rpc_port: u16,
    /// WebSocket 端口，默认 8546。
    pub ws_port: Option<u16>,
    /// 数据目录，默认 `/tank/blockchain/<id>`。
    pub data_dir: String,
    /// `snap` / `full` / `archive`（geth/reth/erigon 用）；dev 链固定 full。
    pub sync_mode: String,
    /// `stopped` / `running` / `syncing` / `error`。
    pub status: String,
    pub enabled: bool,
    pub created_at: String,
    pub error: Option<String>,
    /// 编排产物（构造的 docker-compose.yml，不真实执行）。
    pub compose_yaml: Option<String>,
    /// 生成的启动命令（供用户在宿主机执行）。
    pub start_cmd: Option<String>,
}

/// 区块链浏览器实例（Blockscout）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExplorerInstance {
    pub id: String,
    pub name: String,
    /// 关联哪个 RPC 节点。
    pub node_id: String,
    /// Blockscout Web 端口，默认 4000。
    pub web_port: u16,
    /// Postgres 端口，默认 5432。
    pub db_port: Option<u16>,
    /// `stopped` / `running` / `error`。
    pub status: String,
    /// 访问 URL（`http://localhost:4000`）。
    pub url: Option<String>,
    pub created_at: String,
    pub compose_yaml: Option<String>,
    pub error: Option<String>,
}

/// 链预设（GET /api/v1/blockchain/chain-presets 返回元素）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainPreset {
    pub chain_type: String,
    pub chain_id: u64,
    pub network: String,
    pub name: String,
    /// 该链推荐的客户端列表。
    pub clients: Vec<String>,
    /// 推荐同步模式。
    pub default_sync: String,
}

/// 客户端说明（GET /api/v1/blockchain/clients 返回元素）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub client: String,
    pub name: String,
    pub description: String,
}

/// `GET /api/v1/blockchain/stats` 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockchainStats {
    pub nodes_total: usize,
    pub running: usize,
    pub stopped: usize,
    pub explorers_total: usize,
    pub explorers_running: usize,
    pub supported_chains: usize,
}

/// 创建节点请求体。
#[derive(Debug, Deserialize)]
struct CreateNodeBody {
    name: String,
    #[serde(default)]
    chain_type: Option<String>,
    #[serde(default)]
    chain_id: Option<u64>,
    #[serde(default)]
    network: Option<String>,
    #[serde(default)]
    chain_name: Option<String>,
    #[serde(default)]
    client: Option<String>,
    #[serde(default)]
    rpc_port: Option<u16>,
    #[serde(default)]
    ws_port: Option<u16>,
    #[serde(default)]
    data_dir: Option<String>,
    #[serde(default)]
    sync_mode: Option<String>,
}

/// 创建浏览器请求体。
#[derive(Debug, Deserialize)]
struct CreateExplorerBody {
    name: String,
    node_id: String,
    #[serde(default)]
    web_port: Option<u16>,
    #[serde(default)]
    db_port: Option<u16>,
}

/// 钱包（**API 视图**，绝不包含私钥本体）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Wallet {
    pub id: String,
    pub name: String,
    /// `evm` / `bitcoin` / `custom`。
    pub chain_type: String,
    pub chain_id: u64,
    /// 钱包地址（EVM `0x`+40hex；bitcoin/custom 为降级 hex 占位）。
    pub address: String,
    /// 是否持有私钥（不返回私钥本身）。
    pub has_private_key: bool,
    /// 余额（wei / satoshi 字符串；查询时填充，未查询为 None）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<String>,
    pub created_at: String,
}

/// 钱包落盘结构（含私钥，仅写本地 `wallets.json`，不进任何 API 响应）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WalletRecord {
    id: String,
    name: String,
    chain_type: String,
    chain_id: u64,
    address: String,
    /// 私钥（hex `0x...`）；导入/生成后才有。
    #[serde(skip_serializing_if = "Option::is_none")]
    private_key: Option<String>,
    #[serde(default)]
    balance: Option<String>,
    created_at: String,
}

impl WalletRecord {
    /// 投影为 API 视图（剥离私钥，只留 `has_private_key` 布尔）。
    fn view(&self) -> Wallet {
        Wallet {
            id: self.id.clone(),
            name: self.name.clone(),
            chain_type: self.chain_type.clone(),
            chain_id: self.chain_id,
            address: self.address.clone(),
            has_private_key: self.private_key.is_some(),
            balance: self.balance.clone(),
            created_at: self.created_at.clone(),
        }
    }
}

/// 创建钱包请求体。
#[derive(Debug, Deserialize)]
struct CreateWalletBody {
    name: String,
    #[serde(default)]
    chain_type: Option<String>,
    #[serde(default)]
    chain_id: Option<u64>,
}

/// 导入私钥请求体。
#[derive(Debug, Deserialize)]
struct ImportWalletBody {
    private_key: String,
}

/// 签名交易请求体。
#[derive(Debug, Deserialize)]
struct SignTxBody {
    to: String,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    data: Option<String>,
}

// ----------------------------------------------------------------------------
// 纯函数（docker-compose 构造器，可单测，不执行）
// ----------------------------------------------------------------------------

/// 不同客户端在容器内的数据卷路径（geth/reth/erigon 用 `/root/.ethereum` 等）。
///
/// 返回 `(volume_target, image, entry_cmd_lines)`。entry_cmd_lines 为容器 command 的
/// 逐行片段（已含网络/端口/同步模式参数）。
fn client_recipe(node: &NodeInstance) -> (String, String, Vec<String>) {
    let net = node.chain.network.as_str();
    let sync = node.sync_mode.as_str();
    let cid = node.chain.chain_id;
    match node.client.as_str() {
        "geth" => (
            "/root/.ethereum".into(),
            "ghcr.io/ethereum/client-go:stable".into(),
            vec![
                format!("--{net}"),
                format!("--syncmode={sync}"),
                "--http".into(),
                "--http.addr=0.0.0.0".into(),
                "--http.port=8545".into(),
                "--http.api=eth,net,web3,txpool".into(),
                "--ws".into(),
                "--ws.addr=0.0.0.0".into(),
                "--ws.port=8546".into(),
            ],
        ),
        "reth" => (
            "/root/.local/share/reth".into(),
            "ghcr.io/paradigmxyz/reth:latest".into(),
            vec![
                "node".into(),
                format!("--chain={net}"),
                "--http".into(),
                "--http.addr=0.0.0.0".into(),
                "--http.port=8545".into(),
                "--ws".into(),
                "--ws.addr=0.0.0.0".into(),
                "--ws.port=8546".into(),
            ],
        ),
        "erigon" => (
            "/root/.local/share/erigon".into(),
            "thorax/erigon:latest".into(),
            vec![
                format!("--chain={net}"),
                "--http".into(),
                "--http.addr=0.0.0.0".into(),
                "--http.port=8546".into(),
                format!("--{net}").to_string(), // erigon 兼容 --<network> 旗标
            ],
        ),
        "ganache" => (
            "/data".into(),
            "trufflesuite/ganache:latest".into(),
            vec![
                "-p 8545".into(),
                "-i".into(),
                cid.to_string(),
                "-h 0.0.0.0".into(),
            ],
        ),
        "anvil" => (
            "/root/.foundry".into(),
            "ghcr.io/foundry-rs/foundry:latest".into(),
            vec![
                "anvil".into(),
                "--port 8545".into(),
                format!("--chain-id {cid}"),
                "--host 0.0.0.0".into(),
            ],
        ),
        "op-geth" => (
            "/root/.ethereum".into(),
            "us-docker.pkg.dev/oplabs-tools-artifacts/images/op-geth:latest".into(),
            vec![
                format!("--{net}"),
                "--syncmode=snap".into(),
                "--http".into(),
                "--http.addr=0.0.0.0".into(),
                "--http.port=8545".into(),
                "--ws".into(),
                "--ws.addr=0.0.0.0".into(),
                "--ws.port=8546".into(),
            ],
        ),
        "arbitrum-node" => (
            "/root/.arbitrum".into(),
            "offchainlabs/nitro-node:latest".into(),
            vec![
                "--l1.url=http://localhost:8545".to_string(),
                format!("--chain.id={cid}"),
                "--http.addr=0.0.0.0".into(),
                "--http.port=8545".into(),
            ],
        ),
        _ => (
            "/root/.ethereum".into(),
            "ghcr.io/ethereum/client-go:stable".into(),
            vec![
                format!("--{net}"),
                "--http".into(),
                "--http.port 8545".into(),
            ],
        ),
    }
}

/// 构造 RPC 节点的 docker-compose.yml 内容（纯函数，不执行）。
///
/// 根据 client 生成不同 image / command / 卷挂载：
/// - geth: ghcr.io/ethereum/client-go, `--{network} --syncmode={sync} --http --http.port 8545 --ws --ws.port 8546`
/// - reth: `reth node --chain {network} --http --http.port 8545 --ws --ws.port 8546`
/// - erigon: `--chain {network} --http`
/// - ganache: `-p 8545 -i {chain_id}`
/// - anvil: `anvil --port 8545 --chain-id {chain_id}`
///
/// 端口映射 `rpc_port:8545`, `ws_port:8546`；卷挂载 `data_dir:/root/.ethereum`（或对应客户端路径）。
#[must_use]
pub fn build_node_compose(node: &NodeInstance) -> String {
    let (vol_target, image, cmd_parts) = client_recipe(node);
    let command_yaml = if cmd_parts.is_empty() {
        String::new()
    } else if cmd_parts.len() == 1 {
        format!("      - {}\n", cmd_parts[0])
    } else {
        let mut s = String::new();
        for p in &cmd_parts {
            s.push_str(&format!("      - {p}\n"));
        }
        s
    };
    let ws_block = node
        .ws_port
        .map_or(String::new(), |ws| format!("      - \"{ws}:8546\"\n"));
    let safe_name = node.id.replace([' ', '/', '.'], "-");
    format!(
        "# docker-compose.yml — RPC 节点 {name}（{client} / {net}）\n\
         # 由 OS 区块链管理编排层生成，docker 未安装时请在宿主机手动执行启动命令。\n\
         services:\n\
         \x20 node:\n\
         \x20   image: {image}\n\
         \x20   container_name: os-chain-{safe_name}\n\
         \x20   restart: unless-stopped\n\
         \x20   command:\n{command_yaml}\
         \x20   ports:\n\
         \x20     - \"{rpc}:8545\"\n{ws_block}\
         \x20   volumes:\n\
         \x20     - {data_dir}:{vol_target}\n",
        name = node.name,
        client = node.client,
        net = node.chain.network,
        safe_name = safe_name,
        image = image,
        command_yaml = command_yaml,
        ws_block = ws_block,
        rpc = node.rpc_port,
        data_dir = node.data_dir,
        vol_target = vol_target,
    )
}

/// 构造 Blockscout 浏览器的 docker-compose.yml（纯函数，不执行）。
///
/// Blockscout + Postgres 两服务：`ETHEREUM_JSONRPC_HTTP_URL` 指向关联节点的
/// `http://host.docker.internal:{node.rpc_port}`，`ETHEREUM_JSONRPC_WS_URL` 指向
/// `ws://host.docker.internal:{node.ws_port}`，`DATABASE_URL` 指向同 compose 的
/// postgres，web 端口 `explorer.web_port:4000`。
#[must_use]
pub fn build_explorer_compose(explorer: &ExplorerInstance, node: &NodeInstance) -> String {
    let ws_port = node.ws_port.unwrap_or(8546);
    let db_port = explorer.db_port.unwrap_or(5432);
    let safe_name = explorer.id.replace([' ', '/', '.'], "-");
    format!(
        "# docker-compose.yml — 区块链浏览器 {name}（Blockscout，关联节点 {node_name}）\n\
         # 由 OS 区块链管理编排层生成，docker 未安装时请在宿主机手动执行启动命令。\n\
         services:\n\
         \x20 db:\n\
         \x20   image: postgres:15\n\
         \x20   container_name: os-explorer-{safe_name}-db\n\
         \x20   restart: unless-stopped\n\
         \x20   environment:\n\
         \x20     POSTGRES_USER: blockscout\n\
         \x20     POSTGRES_PASSWORD: blockscout\n\
         \x20     POSTGRES_DB: blockscout\n\
         \x20   ports:\n\
         \x20     - \"{db_port}:5432\"\n\
         \x20   volumes:\n\
         \x20     - {data_dir}/explorer-{safe_name}-db:/var/lib/postgresql/data\n\
         \n\
         \x20 explorer:\n\
         \x20   image: ghcr.io/blockscout/blockscout:latest\n\
         \x20   container_name: os-explorer-{safe_name}\n\
         \x20   restart: unless-stopped\n\
         \x20   depends_on:\n\
         \x20     - db\n\
         \x20   ports:\n\
         \x20     - \"{web}:4000\"\n\
         \x20   environment:\n\
         \x20     ETHEREUM_JSONRPC_HTTP_URL: http://host.docker.internal:{rpc}\n\
         \x20     ETHEREUM_JSONRPC_WS_URL: ws://host.docker.internal:{ws_port}\n\
         \x20     ETHEREUM_JSONRPC_TRACE_URL: http://host.docker.internal:{rpc}\n\
         \x20     DATABASE_URL: postgresql://blockscout:blockscout@db:5432/blockscout\n\
         \x20     BLOCKSCOUT_HOST: localhost\n\
         \x20     PORT: \"4000\"\n",
        name = explorer.name,
        node_name = node.name,
        safe_name = safe_name,
        db_port = db_port,
        data_dir = node.data_dir,
        web = explorer.web_port,
        rpc = node.rpc_port,
        ws_port = ws_port,
    )
}

/// 构造节点启动命令（供用户在宿主机执行）。
///
/// 形如：`mkdir -p <data_dir> && cd <data_dir> && docker compose up -d`
#[must_use]
pub fn build_node_start_cmd(node: &NodeInstance) -> String {
    format!(
        "mkdir -p {data} && cd {data} && docker compose up -d",
        data = node.data_dir,
    )
}

/// 构造浏览器启动命令（供用户在宿主机执行）。
#[must_use]
pub fn build_explorer_start_cmd(explorer: &ExplorerInstance) -> String {
    format!(
        "cd /tank/blockchain && docker compose -p explorer-{id} up -d",
        id = explorer.id.replace([' ', '/', '.'], "-"),
    )
}

// ----------------------------------------------------------------------------
// 钱包纯函数（脚本构造器 / curl 构造器 / 脱敏 / 落盘 IO，可单测，不执行）
// ----------------------------------------------------------------------------

/// 钱包列表落盘路径。
const WALLETS_FILE: &str = "/tank/os-data/wallets.json";

/// 钱包密钥学（rustify：`k256` secp256k1 + `tiny-keccak` keccak256 纯 Rust 实现，
/// 替代原 python3 内置 keccak-256 + 点乘脚本子进程）。
///
/// 地址派生规则与原脚本逐字节一致：
/// - `evm`：`0x` + keccak256(未压缩公钥[1..])[12..]（40 hex 小写）
/// - `bitcoin` / 其他：`0x` + sha256(公钥[1..])[:20]（降级占位，真实 BTC 编码留后续）
fn derive_address_from_secret(secret: &k256::ecdsa::SigningKey, chain_type: &str) -> String {
    use sha2::Digest;
    use tiny_keccak::{Hasher, Keccak};
    let public = secret.verifying_key().to_encoded_point(false); // 未压缩 65B
    let pubkey_no_prefix: [u8; 64] = public.as_bytes()[1..].try_into().expect("64 字节公钥");
    let digest: [u8; 20] = if chain_type == "evm" {
        let mut hasher = Keccak::v256();
        hasher.update(&pubkey_no_prefix);
        let mut hash = [0u8; 32];
        hasher.finalize(&mut hash);
        hash[12..].try_into().expect("20 字节地址")
    } else {
        let mut h = sha2::Sha256::new();
        h.update(pubkey_no_prefix);
        let hash: [u8; 32] = h.finalize().into();
        hash[..20].try_into().expect("20 字节地址")
    };
    format!("0x{}", hex::encode(digest))
}

/// 随机生成钱包密钥对（secp256k1 CSPRNG）。返回 `(私钥hex(0x+64), 地址)`。
fn generate_wallet_keypair(chain_type: &str) -> Result<(String, String), String> {
    let sk = k256::ecdsa::SigningKey::random(&mut k256::elliptic_curve::rand_core::OsRng);
    let addr = derive_address_from_secret(&sk, chain_type);
    Ok((format!("0x{}", hex::encode(sk.to_bytes())), addr))
}

/// 由私钥 hex（可带 0x 前缀）派生地址（导入用）。
/// 非法 hex / 非 32 字节 / secp256k1 域外 → Err（caller 降级报错，不 panic）。
fn derive_address_from_private_key(priv_hex: &str, chain_type: &str) -> Result<String, String> {
    let clean = priv_hex
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    let bytes = hex::decode(clean).map_err(|e| format!("私钥 hex 解码失败: {e}"))?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|v: Vec<u8>| format!("私钥应为 32 字节（64 hex），实际 {} 字节", v.len()))?;
    let sk = k256::ecdsa::SigningKey::from_bytes(&bytes.into())
        .map_err(|e| format!("私钥无效（secp256k1 域外）: {e}"))?;
    Ok(derive_address_from_secret(&sk, chain_type))
}

/// 签名交易 Python 脚本：优先 `eth_account`（web3.py 工具链）本地签名；
/// 库不可用时**降级**返回未签名交易构造（`signed:false` + `unsigned_tx`），不 panic。
///
/// 用法：`python3 -c <script> <私钥hex> <to> <value> <data> <chain_id>`
const WALLET_SIGN_SCRIPT: &str = r#"
import json, sys
priv, to, value, data, chain_id = (sys.argv[1:6] + ['', '', '0', '0x', '1'])[:5]
tx = {'nonce': 0, 'gasPrice': 0, 'gas': 21000, 'to': to, 'value': int(value or '0'),
      'data': data or '0x', 'chainId': int(chain_id or '1')}
try:
    from eth_account import Account
    signed = Account.sign_transaction(tx, priv)
    def h(v):
        s = v.hex() if hasattr(v, 'hex') else str(v)
        return s if s.startswith('0x') else '0x' + s
    print(json.dumps({'signed': True, 'raw_transaction': h(signed.raw_transaction),
                      'tx_hash': h(signed.hash), 'unsigned_tx': tx}))
except Exception as e:
    print(json.dumps({'signed': False,
                      'reason': 'eth_account/web3 不可用，已降级为未签名交易: %s' % e,
                      'unsigned_tx': tx}))
"#;

/// 构造 `eth_getBalance` 的 JSON-RPC 请求体（rustify：原 curl 命令构造器改为
/// 纯 JSON payload，由共享 reqwest Client POST 到 RPC 节点）。
#[must_use]
pub fn build_balance_payload(address: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_getBalance",
        "params": [address, "latest"],
        "id": 1
    })
}

/// reqwest POST `eth_getBalance`（节点离线/无响应 → None，降级不 panic）。
async fn fetch_balance_rpc(rpc_url: &str, address: &str) -> Option<String> {
    let resp = HTTP
        .post(rpc_url)
        .timeout(Duration::from_secs(5))
        .json(&build_balance_payload(address))
        .send()
        .await
        .ok()?;
    let out = resp.text().await.ok()?;
    parse_balance_output(true, &out)
}

/// 地址脱敏：`0x1234...5678`（短地址原样返回）。
#[must_use]
pub fn mask_address(addr: &str) -> String {
    let a = addr.trim();
    if a.len() <= 12 {
        return a.to_string();
    }
    format!("{}...{}", &a[..6], &a[a.len() - 4..])
}

/// 从 `eth_getBalance` 的 curl stdout 解析余额（wei hex 字符串）。
/// 非 JSON / 无 result 字段（节点离线、curl 失败）→ `None`（降级不 panic）。
fn parse_balance_output(success: bool, out: &str) -> Option<String> {
    if !success {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(out)
        .ok()?
        .get("result")?
        .as_str()
        .map(str::to_string)
}

/// 从签名脚本 stdout 解析 JSON 值；失败 → `None`（caller 降级）。
fn parse_json_output(success: bool, out: &str) -> Option<serde_json::Value> {
    if !success {
        return None;
    }
    serde_json::from_str(out.lines().last()?).ok()
}

/// 阻塞读钱包落盘文件（不存在/损坏 → 空 vec，降级不 panic）。
fn load_wallets_blocking(path: &str) -> Vec<WalletRecord> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// 阻塞写钱包落盘文件（先建目录；失败返回 Err，caller 降级忽略）。
fn save_wallets_blocking(path: &str, wallets: &[WalletRecord]) -> Result<(), String> {
    if let Some(parent) = std::path::Path::new(path).parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建 {parent:?} 失败: {e}"))?;
    }
    let json = serde_json::to_string_pretty(wallets).map_err(|e| format!("钱包序列化失败: {e}"))?;
    std::fs::write(path, json).map_err(|e| format!("写入 {path} 失败: {e}"))
}

/// 返回 4 类链的预设配置列表（chain-presets）。
#[must_use]
pub fn chain_presets() -> Vec<ChainPreset> {
    vec![
        ChainPreset {
            chain_type: "ethereum".into(),
            chain_id: 1,
            network: "mainnet".into(),
            name: "Ethereum 主网".into(),
            clients: vec!["geth".into(), "reth".into(), "erigon".into()],
            default_sync: "snap".into(),
        },
        ChainPreset {
            chain_type: "ethereum".into(),
            chain_id: 11_155_111,
            network: "sepolia".into(),
            name: "Sepolia 测试网".into(),
            clients: vec!["geth".into(), "reth".into()],
            default_sync: "snap".into(),
        },
        ChainPreset {
            chain_type: "dev".into(),
            chain_id: 1337,
            network: "dev".into(),
            name: "本地开发链".into(),
            clients: vec!["ganache".into(), "anvil".into()],
            default_sync: "full".into(),
        },
        ChainPreset {
            chain_type: "l2".into(),
            chain_id: 10,
            network: "optimism".into(),
            name: "Optimism".into(),
            clients: vec!["op-geth".into()],
            default_sync: "snap".into(),
        },
        ChainPreset {
            chain_type: "l2".into(),
            chain_id: 42_161,
            network: "arbitrum".into(),
            name: "Arbitrum One".into(),
            clients: vec!["arbitrum-node".into()],
            default_sync: "snap".into(),
        },
        ChainPreset {
            chain_type: "l2".into(),
            chain_id: 8453,
            network: "base".into(),
            name: "Base".into(),
            clients: vec!["op-geth".into()],
            default_sync: "snap".into(),
        },
        ChainPreset {
            chain_type: "custom".into(),
            chain_id: 0,
            network: "custom".into(),
            name: "自定义私有链".into(),
            clients: vec!["geth".into(), "reth".into()],
            default_sync: "full".into(),
        },
    ]
}

/// 返回支持的客户端列表 + 各自说明（clients）。
#[must_use]
pub fn supported_clients() -> Vec<ClientInfo> {
    vec![
        ClientInfo {
            client: "geth".into(),
            name: "Geth (Go-Ethereum)".into(),
            description:
                "最广泛使用的以太坊执行客户端，支持 snap/full/archive 同步，适合主网/测试网全节点。"
                    .into(),
        },
        ClientInfo {
            client: "reth".into(),
            name: "Reth (Paradigm)".into(),
            description:
                "Rust 实现的高性能以太坊执行客户端，存档节点资源占用低，适合存储受限环境。".into(),
        },
        ClientInfo {
            client: "erigon".into(),
            name: "Erigon".into(),
            description: "Go 实现的以太坊客户端，以高效存档同步著称，适合需要完整历史数据的场景。"
                .into(),
        },
        ClientInfo {
            client: "ganache".into(),
            name: "Ganache".into(),
            description:
                "本地开发链（Truffle Suite），即时出块、可确定性 fork 主网，适合 DApp 开发调试。"
                    .into(),
        },
        ClientInfo {
            client: "anvil".into(),
            name: "Anvil (Foundry)".into(),
            description: "Foundry 工具链的本地开发链，启动快、支持 fork 主网，适合合约开发与测试。"
                .into(),
        },
        ClientInfo {
            client: "op-geth".into(),
            name: "op-geth (Optimism)".into(),
            description:
                "Optimism Stack 的执行客户端，用于运行 Optimism / Base 等 OP Stack L2 节点。".into(),
        },
        ClientInfo {
            client: "arbitrum-node".into(),
            name: "Arbitrum Nitro".into(),
            description: "Arbitrum One 的全节点客户端（Nitro），用于运行 Arbitrum L2 节点。".into(),
        },
    ]
}

// ----------------------------------------------------------------------------
// BlockchainRouteHandler
// ----------------------------------------------------------------------------

/// 区块链管理路由处理器——HTTP 边界适配到内存态节点/浏览器列表。
///
/// 编排层语义：构造 docker-compose + 启动命令字符串，不真实 spawn docker
/// （docker 未安装也不 panic）。启动/停止操作仅切换 `status`。
pub struct BlockchainRouteHandler {
    nodes: Mutex<Vec<NodeInstance>>,
    explorers: Mutex<Vec<ExplorerInstance>>,
    wallets: Mutex<Vec<WalletRecord>>,
    counter: Mutex<u64>,
    /// 钱包落盘路径（None = 不落盘，测试注入用）。
    wallets_file: Option<String>,
    /// 节点运行管理子模块状态（/api/v1/blockchain/chain-nodes*：geth/bitcoind
    /// 真实子进程生命周期 + 空间预检；见 blockchain_nodes.rs 模块头）。
    chain_node_state: ChainNodeState,
}

impl BlockchainRouteHandler {
    /// 构造 handler，预置 demo 节点/浏览器，钱包从落盘文件恢复。
    #[must_use]
    pub fn new() -> Self {
        let stored = load_wallets_blocking(WALLETS_FILE);
        Self {
            nodes: Mutex::new(demo_nodes()),
            explorers: Mutex::new(vec![]),
            wallets: Mutex::new(stored),
            counter: Mutex::new(100),
            wallets_file: Some(WALLETS_FILE.to_string()),
            chain_node_state: ChainNodeState::new(),
        }
    }

    /// 用空列表构造（测试注入；不读不写落盘文件）。
    #[must_use]
    pub fn with_empty() -> Self {
        Self {
            nodes: Mutex::new(vec![]),
            explorers: Mutex::new(vec![]),
            wallets: Mutex::new(vec![]),
            counter: Mutex::new(100),
            wallets_file: None,
            chain_node_state: ChainNodeState::in_memory(),
        }
    }

    /// 当前全量钱包快照（API 视图，无私钥）。
    #[must_use]
    pub fn wallets_snapshot(&self) -> Vec<Wallet> {
        self.wallets
            .lock()
            .expect("wallets poisoned")
            .iter()
            .map(WalletRecord::view)
            .collect()
    }

    /// 钱包落盘（spawn_blocking；无路径或写失败 → 降级忽略，不影响内存态）。
    fn persist_wallets(&self) {
        let Some(path) = self.wallets_file.clone() else {
            return;
        };
        let snapshot = self.wallets.lock().expect("wallets poisoned").clone();
        tokio::spawn(async move {
            let _ =
                tokio::task::spawn_blocking(move || save_wallets_blocking(&path, &snapshot)).await;
        });
    }

    /// 通用 spawn 命令（stdout/stderr 合并诊断），失败降级 `(false, 原因)` 不 panic。
    async fn spawn_command(cmd: &[String]) -> (bool, String) {
        if cmd.is_empty() {
            return (false, "空命令".into());
        }
        let mut c = tokio::process::Command::new(&cmd[0]);
        c.args(&cmd[1..]);
        c.stdout(std::process::Stdio::piped());
        c.stderr(std::process::Stdio::piped());
        c.stdin(std::process::Stdio::null());
        match c.output().await {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let combined = if stderr.is_empty() {
                    stdout
                } else if stdout.is_empty() {
                    stderr
                } else {
                    format!("{stdout} | {stderr}")
                };
                (out.status.success(), combined)
            }
            Err(e) => (false, format!("`{}` 调用失败（未安装？）: {e}", cmd[0])),
        }
    }

    /// spawn `python3 -c <script> <args..>`（密钥生成/派生/签名共用）。
    async fn spawn_python(script: &str, args: &[&str]) -> (bool, String) {
        let mut cmd = vec!["python3".to_string(), "-c".to_string(), script.to_string()];
        cmd.extend(args.iter().map(|s| (*s).to_string()));
        Self::spawn_command(&cmd).await
    }

    /// 当前全量节点快照。
    #[must_use]
    pub fn nodes_snapshot(&self) -> Vec<NodeInstance> {
        self.nodes.lock().expect("nodes poisoned").clone()
    }

    /// 当前全量浏览器快照。
    #[must_use]
    pub fn explorers_snapshot(&self) -> Vec<ExplorerInstance> {
        self.explorers.lock().expect("explorers poisoned").clone()
    }

    fn next_id(&self, prefix: &str) -> String {
        let mut c = self.counter.lock().expect("counter poisoned");
        *c += 1;
        format!("{prefix}-{}", *c)
    }

    /// 根据请求体补全 ChainConfig（按 network 反查预设，缺失字段用默认）。
    fn resolve_chain(body: &CreateNodeBody) -> Result<ChainConfig, String> {
        let presets = chain_presets();
        // 优先用请求体显式字段；否则按 network 反查预设；再否则用 chain_type 推断默认。
        let network = body
            .network
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| match body.chain_type.as_deref().unwrap_or("ethereum") {
                "dev" => "dev".into(),
                "l2" => "optimism".into(),
                "custom" => "custom".into(),
                _ => "mainnet".into(),
            });
        let preset = presets.iter().find(|p| p.network == network);
        let chain_type = body
            .chain_type
            .clone()
            .or_else(|| preset.map(|p| p.chain_type.clone()))
            .unwrap_or_else(|| "ethereum".into());
        let chain_id = body
            .chain_id
            .or_else(|| preset.map(|p| p.chain_id))
            .unwrap_or(1);
        let name = body
            .chain_name
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| preset.map(|p| p.name.clone()))
            .unwrap_or_else(|| network.clone());
        Ok(ChainConfig {
            chain_type,
            chain_id,
            network,
            name,
        })
    }

    /// 落盘 docker-compose.yml 到 `data_dir/docker-compose.yml`（spawn_blocking）。
    ///
    /// 先 `create_dir_all(data_dir)`，再写文件。data_dir 不可写（如 /tank 不存在）
    /// 时返回 Err（caller 降级为 error 状态）。compose_yaml 为 None 时跳过写入
    /// 并返回 Err（无 compose 可启动）。
    fn write_compose_blocking(data_dir: &str, compose_yaml: &Option<String>) -> Result<(), String> {
        let yaml = compose_yaml
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "docker-compose.yml 内容为空".to_string())?;
        std::fs::create_dir_all(data_dir)
            .map_err(|e| format!("创建数据目录 {data_dir} 失败: {e}"))?;
        let path = std::path::Path::new(data_dir).join("docker-compose.yml");
        std::fs::write(&path, yaml).map_err(|e| format!("写入 {path:?} 失败: {e}"))?;
        Ok(())
    }

    /// 真实 spawn `docker compose <args>`（cwd=data_dir），等待完成并返回（success, combined_output）。
    ///
    /// docker 不存在 / 权限不足 / compose 失败 → 返回 (false, <原因>)，不 panic。
    /// 用 tokio::process::Command（caller 在 async 上下文调用）。
    async fn spawn_docker_compose(data_dir: &str, args: &[&str]) -> (bool, String) {
        // 优先直接 docker；权限组问题留给 docker 自身报错（caller 降级）。
        let mut cmd = tokio::process::Command::new("docker");
        cmd.arg("compose").args(args);
        cmd.current_dir(data_dir);
        // 静默 stdout，stderr 透传到 output 以便诊断
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
        cmd.stdin(std::process::Stdio::null());
        match cmd.output().await {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                let combined = if stderr.is_empty() {
                    stdout
                } else if stdout.is_empty() {
                    stderr
                } else {
                    format!("{stdout} | {stderr}")
                };
                (out.status.success(), combined)
            }
            Err(e) => (false, format!("docker 调用失败（未安装？）: {e}")),
        }
    }
}

impl Default for BlockchainRouteHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RouteHandler for BlockchainRouteHandler {
    async fn routes(&self) -> Vec<RouteSpec> {
        vec![
            // 节点管理（6 条）
            spec(HttpMethod::Get, "/api/v1/blockchain/nodes", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/blockchain/nodes",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/blockchain/nodes/:id",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/blockchain/nodes/:id/start",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/blockchain/nodes/:id/stop",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/blockchain/nodes/:id",
                true,
                vec!["admin".into()],
            ),
            // 浏览器管理（4 条）
            spec(
                HttpMethod::Get,
                "/api/v1/blockchain/explorers",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/blockchain/explorers",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/blockchain/explorers/:id",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/blockchain/explorers/:id/start",
                true,
                vec!["admin".into()],
            ),
            // 链预设 + 统计（3 条）
            spec(
                HttpMethod::Get,
                "/api/v1/blockchain/chain-presets",
                false,
                vec![],
            ),
            spec(HttpMethod::Get, "/api/v1/blockchain/stats", false, vec![]),
            spec(HttpMethod::Get, "/api/v1/blockchain/clients", false, vec![]),
            // 钱包管理（7 条）
            spec(HttpMethod::Get, "/api/v1/blockchain/wallets", false, vec![]),
            spec(
                HttpMethod::Post,
                "/api/v1/blockchain/wallets",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Delete,
                "/api/v1/blockchain/wallets/:id",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/blockchain/wallets/:id/balance",
                false,
                vec![],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/blockchain/wallets/:id/import",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Post,
                "/api/v1/blockchain/wallets/:id/sign",
                true,
                vec!["admin".into()],
            ),
            spec(
                HttpMethod::Get,
                "/api/v1/blockchain/wallets/:id/address",
                false,
                vec![],
            ),
        ]
        // 节点运行管理子模块（/api/v1/blockchain/chain-nodes*，9 条：列表/创建/
        // 预设/空间预检/详情/启停/删除/日志——specs 同源，handle 侧整体委托）
        .into_iter()
        .chain(blockchain_nodes::route_specs())
        .collect()
    }

    async fn handle(&self, req: ApiRequest) -> Result<ApiResponse, ApiGatewayError> {
        let segs = path_segments(&req.path);
        // —— /api/v1/blockchain/chain-nodes* —— 节点运行管理（整体委托
        // blockchain_nodes 子模块：geth/bitcoind 真实子进程生命周期 + 空间
        // 预检 + 预设/日志；路由 specs 同源 blockchain_nodes::route_specs，
        // 契约见该模块头与 docs/BLOCKCHAIN_NODES.md）
        if matches!(segs.as_slice(), ["api", "v1", "blockchain", "chain-nodes", ..]) {
            let query = req.path.split('?').nth(1).unwrap_or("");
            return blockchain_nodes::handle(
                &self.chain_node_state,
                req.method,
                &segs[4..],
                query,
                req.body,
            )
            .await;
        }
        match (req.method, segs.as_slice()) {
            // ============ 节点管理 ============

            // —— GET /api/v1/blockchain/nodes —— 列节点
            (HttpMethod::Get, ["api", "v1", "blockchain", "nodes"]) => {
                Ok(ok_json(to_value(&self.nodes_snapshot())?))
            }

            // —— POST /api/v1/blockchain/nodes —— 创建节点（admin）→ 构造 compose + start_cmd
            (HttpMethod::Post, ["api", "v1", "blockchain", "nodes"]) => {
                let body: CreateNodeBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析创建节点请求体失败: {e}"))
                })?;
                if body.name.trim().is_empty() {
                    return Ok(error_response(400, "name 不可为空"));
                }
                let chain = match Self::resolve_chain(&body) {
                    Ok(c) => c,
                    Err(e) => return Ok(error_response(400, &e)),
                };
                let client = body.client.filter(|s| !s.is_empty()).unwrap_or_else(|| {
                    match chain.chain_type.as_str() {
                        "dev" => "anvil".into(),
                        "l2" if chain.network == "arbitrum" => "arbitrum-node".into(),
                        "l2" => "op-geth".into(),
                        _ => "geth".into(),
                    }
                });
                let id = self.next_id("node");
                let data_dir = body
                    .data_dir
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| format!("/tank/blockchain/{id}"));
                // dev 链固定 full；其余按请求或预设默认。
                let sync_mode = if chain.chain_type == "dev" {
                    "full".to_string()
                } else {
                    body.sync_mode.filter(|s| !s.is_empty()).unwrap_or_else(|| {
                        chain_presets()
                            .iter()
                            .find(|p| p.network == chain.network)
                            .map(|p| p.default_sync.clone())
                            .unwrap_or_else(|| "snap".into())
                    })
                };
                let mut node = NodeInstance {
                    id: id.clone(),
                    name: body.name,
                    chain,
                    client,
                    rpc_port: body.rpc_port.unwrap_or(8545),
                    ws_port: body.ws_port.or(Some(8546)),
                    data_dir,
                    sync_mode,
                    status: "stopped".into(),
                    enabled: true,
                    created_at: now_iso(),
                    error: None,
                    compose_yaml: None,
                    start_cmd: None,
                };
                let compose = build_node_compose(&node);
                let cmd = build_node_start_cmd(&node);
                node.compose_yaml = Some(compose);
                node.start_cmd = Some(cmd);
                let resp = to_value(&node)?;
                self.nodes.lock().expect("nodes poisoned").push(node);
                Ok(ApiResponse {
                    status: 201,
                    body: resp,
                    headers: serde_json::json!({}),
                })
            }

            // —— GET /api/v1/blockchain/nodes/:id —— 单节点详情（含 compose_yaml）
            (HttpMethod::Get, ["api", "v1", "blockchain", "nodes", id]) => {
                let nodes = self.nodes.lock().expect("nodes poisoned");
                match nodes.iter().find(|n| n.id == *id) {
                    Some(n) => Ok(ok_json(to_value(n)?)),
                    None => Ok(error_response(404, &format!("节点不存在: {id}"))),
                }
            }

            // —— POST /api/v1/blockchain/nodes/:id/start —— 真实启动（admin，spawn docker）
            (HttpMethod::Post, ["api", "v1", "blockchain", "nodes", id, "start"]) => {
                // 先快照节点（锁立即释放，避免在 await 期间持锁）
                let snap = {
                    let nodes = self.nodes.lock().expect("nodes poisoned");
                    nodes.iter().find(|n| n.id == *id).cloned()
                };
                let Some(n) = snap else {
                    return Ok(error_response(404, &format!("节点不存在: {id}")));
                };
                // 1) 落盘 docker-compose.yml（spawn_blocking）
                let data_dir = n.data_dir.clone();
                let compose = n.compose_yaml.clone();
                let write_err = tokio::task::spawn_blocking(move || {
                    Self::write_compose_blocking(&data_dir, &compose)
                })
                .await
                .ok()
                .and_then(|r| r.err());
                // 2) 真实 spawn docker compose up -d（失败降级 error）
                let (success, detail) = if let Some(e) = write_err {
                    (false, e)
                } else {
                    Self::spawn_docker_compose(&n.data_dir, &["up", "-d"]).await
                };
                // 3) 回写状态
                let mut nodes = self.nodes.lock().expect("nodes poisoned");
                if let Some(node) = nodes.iter_mut().find(|x| x.id == *id) {
                    node.status = if success {
                        "running".into()
                    } else {
                        "error".into()
                    };
                    node.error = if success { None } else { Some(detail.clone()) };
                }
                if success {
                    Ok(ok_json(serde_json::json!({
                        "ok": true,
                        "id": id,
                        "status": "running",
                        "docker": true,
                        "note": "已真实 spawn docker compose up -d",
                    })))
                } else {
                    Ok(error_response(
                        502,
                        &format!("docker compose up -d 失败: {detail}"),
                    ))
                }
            }

            // —— POST /api/v1/blockchain/nodes/:id/stop —— 真实停止（admin，spawn docker）
            (HttpMethod::Post, ["api", "v1", "blockchain", "nodes", id, "stop"]) => {
                let snap = {
                    let nodes = self.nodes.lock().expect("nodes poisoned");
                    nodes.iter().find(|n| n.id == *id).cloned()
                };
                let Some(n) = snap else {
                    return Ok(error_response(404, &format!("节点不存在: {id}")));
                };
                // docker compose down（失败也标记 stopped，仅记录 error）
                let (success, detail) = Self::spawn_docker_compose(&n.data_dir, &["down"]).await;
                let mut nodes = self.nodes.lock().expect("nodes poisoned");
                if let Some(node) = nodes.iter_mut().find(|x| x.id == *id) {
                    node.status = "stopped".into();
                    node.error = if success { None } else { Some(detail.clone()) };
                }
                if success {
                    Ok(ok_json(serde_json::json!({
                        "ok": true,
                        "id": id,
                        "status": "stopped",
                        "docker": true,
                        "note": "已真实 spawn docker compose down",
                    })))
                } else {
                    // stop 失败不致命：节点仍标记 stopped，仅回告警
                    Ok(ok_json(serde_json::json!({
                        "ok": true,
                        "id": id,
                        "status": "stopped",
                        "docker": false,
                        "warning": format!("docker compose down 失败（已标记 stopped）: {detail}"),
                    })))
                }
            }

            // —— DELETE /api/v1/blockchain/nodes/:id —— 删节点（admin）
            (HttpMethod::Delete, ["api", "v1", "blockchain", "nodes", id]) => {
                let mut nodes = self.nodes.lock().expect("nodes poisoned");
                let before = nodes.len();
                nodes.retain(|n| n.id != *id);
                if nodes.len() == before {
                    return Ok(error_response(404, &format!("节点不存在: {id}")));
                }
                // 同步清理关联浏览器
                let mut explorers = self.explorers.lock().expect("explorers poisoned");
                explorers.retain(|e| e.node_id != *id);
                Ok(ok_json(
                    serde_json::json!({"ok": true, "id": id, "action": "delete"}),
                ))
            }

            // ============ 浏览器管理 ============

            // —— GET /api/v1/blockchain/explorers —— 列浏览器
            (HttpMethod::Get, ["api", "v1", "blockchain", "explorers"]) => {
                Ok(ok_json(to_value(&self.explorers_snapshot())?))
            }

            // —— POST /api/v1/blockchain/explorers —— 创建浏览器（admin）→ 关联 node_id
            (HttpMethod::Post, ["api", "v1", "blockchain", "explorers"]) => {
                let body: CreateExplorerBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析创建浏览器请求体失败: {e}"))
                })?;
                if body.name.trim().is_empty() {
                    return Ok(error_response(400, "name 不可为空"));
                }
                if body.node_id.trim().is_empty() {
                    return Ok(error_response(400, "node_id 不可为空"));
                }
                let node_snap = {
                    let nodes = self.nodes.lock().expect("nodes poisoned");
                    nodes.iter().find(|n| n.id == body.node_id).cloned()
                };
                let Some(node) = node_snap else {
                    return Ok(error_response(
                        404,
                        &format!("关联节点不存在: {}", body.node_id),
                    ));
                };
                let id = self.next_id("explorer");
                let web_port = body.web_port.unwrap_or(4000);
                let mut explorer = ExplorerInstance {
                    id: id.clone(),
                    name: body.name,
                    node_id: body.node_id,
                    web_port,
                    db_port: body.db_port.or(Some(5432)),
                    status: "stopped".into(),
                    url: Some(format!("http://localhost:{web_port}")),
                    created_at: now_iso(),
                    compose_yaml: None,
                    error: None,
                };
                let compose = build_explorer_compose(&explorer, &node);
                explorer.compose_yaml = Some(compose);
                let resp = to_value(&explorer)?;
                self.explorers
                    .lock()
                    .expect("explorers poisoned")
                    .push(explorer);
                Ok(ApiResponse {
                    status: 201,
                    body: resp,
                    headers: serde_json::json!({}),
                })
            }

            // —— DELETE /api/v1/blockchain/explorers/:id —— 删浏览器（admin）
            (HttpMethod::Delete, ["api", "v1", "blockchain", "explorers", id]) => {
                let mut explorers = self.explorers.lock().expect("explorers poisoned");
                let before = explorers.len();
                explorers.retain(|e| e.id != *id);
                if explorers.len() == before {
                    return Ok(error_response(404, &format!("浏览器不存在: {id}")));
                }
                Ok(ok_json(
                    serde_json::json!({"ok": true, "id": id, "action": "delete"}),
                ))
            }

            // —— POST /api/v1/blockchain/explorers/:id/start —— 真实启动（admin，spawn docker）
            (HttpMethod::Post, ["api", "v1", "blockchain", "explorers", id, "start"]) => {
                let snap = {
                    let explorers = self.explorers.lock().expect("explorers poisoned");
                    explorers.iter().find(|e| e.id == *id).cloned()
                };
                let Some(e) = snap else {
                    return Ok(error_response(404, &format!("浏览器不存在: {id}")));
                };
                // 浏览器专属工作目录：/tank/blockchain/<explorer-id>
                let work_dir = format!("/tank/blockchain/{}", e.id);
                let compose = e.compose_yaml.clone();
                // 1) 落盘 compose（spawn_blocking）
                let write_err = tokio::task::spawn_blocking(move || {
                    Self::write_compose_blocking(&work_dir, &compose)
                })
                .await
                .ok()
                .and_then(|r| r.err());
                let work_dir = format!("/tank/blockchain/{}", e.id);
                // 2) 真实 spawn docker compose up -d
                let (success, detail) = if let Some(err) = write_err {
                    (false, err)
                } else {
                    Self::spawn_docker_compose(&work_dir, &["up", "-d"]).await
                };
                // 3) 回写状态
                let mut explorers = self.explorers.lock().expect("explorers poisoned");
                let url = explorers
                    .iter()
                    .find(|x| x.id == *id)
                    .and_then(|x| x.url.clone());
                if let Some(ex) = explorers.iter_mut().find(|x| x.id == *id) {
                    ex.status = if success {
                        "running".into()
                    } else {
                        "error".into()
                    };
                    ex.error = if success { None } else { Some(detail.clone()) };
                }
                if success {
                    Ok(ok_json(serde_json::json!({
                        "ok": true,
                        "id": id,
                        "status": "running",
                        "url": url,
                        "docker": true,
                        "note": "已真实 spawn docker compose up -d",
                    })))
                } else {
                    Ok(error_response(
                        502,
                        &format!("docker compose up -d 失败: {detail}"),
                    ))
                }
            }

            // ============ 链预设 + 统计 + 客户端 ============

            // —— GET /api/v1/blockchain/chain-presets —— 4 类链的预设配置
            (HttpMethod::Get, ["api", "v1", "blockchain", "chain-presets"]) => {
                Ok(ok_json(to_value(&chain_presets())?))
            }

            // —— GET /api/v1/blockchain/stats —— 聚合统计
            (HttpMethod::Get, ["api", "v1", "blockchain", "stats"]) => {
                let nodes = self.nodes.lock().expect("nodes poisoned");
                let explorers = self.explorers.lock().expect("explorers poisoned");
                let running = nodes.iter().filter(|n| n.status == "running").count();
                let stopped = nodes.iter().filter(|n| n.status == "stopped").count();
                let explorers_running = explorers.iter().filter(|e| e.status == "running").count();
                Ok(ok_json(to_value(&BlockchainStats {
                    nodes_total: nodes.len(),
                    running,
                    stopped,
                    explorers_total: explorers.len(),
                    explorers_running,
                    supported_chains: chain_presets().len(),
                })?))
            }

            // —— GET /api/v1/blockchain/clients —— 支持的客户端列表
            (HttpMethod::Get, ["api", "v1", "blockchain", "clients"]) => {
                Ok(ok_json(to_value(&supported_clients())?))
            }

            // ============ 钱包管理 ============

            // —— GET /api/v1/blockchain/wallets —— 列钱包（不含私钥）
            (HttpMethod::Get, ["api", "v1", "blockchain", "wallets"]) => {
                Ok(ok_json(to_value(&self.wallets_snapshot())?))
            }

            // —— POST /api/v1/blockchain/wallets —— 创建钱包（admin，纯 Rust 生成密钥对）
            (HttpMethod::Post, ["api", "v1", "blockchain", "wallets"]) => {
                let body: CreateWalletBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析创建钱包请求体失败: {e}"))
                })?;
                if body.name.trim().is_empty() {
                    return Ok(error_response(400, "name 不可为空"));
                }
                let chain_type = body
                    .chain_type
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "evm".into());
                let chain_id = body.chain_id.unwrap_or(match chain_type.as_str() {
                    "dev" => 1337,
                    "l2" => 8453,
                    "bitcoin" => 0,
                    _ => 1,
                });
                // rustify：k256 + tiny-keccak 纯 Rust 生成密钥对（原 python3 子进程移除）；
                // 失败降级：占位地址 + 无私钥 + warning（不 panic）
                let (address, private_key, warning) = match generate_wallet_keypair(&chain_type) {
                    Ok((key, addr)) => (addr, Some(key), None),
                    Err(e) => {
                        let addr = format!(
                            "0x{:040x}",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_nanos())
                                .unwrap_or(0)
                        );
                        (addr, None, Some(format!("密钥对生成降级（占位地址）: {e}")))
                    }
                };
                let id = self.next_id("wallet");
                let rec = WalletRecord {
                    id: id.clone(),
                    name: body.name,
                    chain_type,
                    chain_id,
                    address,
                    private_key,
                    balance: None,
                    created_at: now_iso(),
                };
                let mut resp_body = to_value(&rec.view())?;
                if let Some(w) = warning {
                    resp_body["warning"] = serde_json::Value::String(w);
                }
                let resp_body = resp_body; // 逃逸帮助：显式结束可变借用
                self.wallets.lock().expect("wallets poisoned").push(rec);
                self.persist_wallets();
                Ok(ApiResponse {
                    status: 201,
                    body: resp_body,
                    headers: serde_json::json!({}),
                })
            }

            // —— DELETE /api/v1/blockchain/wallets/:id —— 删钱包（admin）
            (HttpMethod::Delete, ["api", "v1", "blockchain", "wallets", id]) => {
                let mut wallets = self.wallets.lock().expect("wallets poisoned");
                let before = wallets.len();
                wallets.retain(|w| w.id != *id);
                if wallets.len() == before {
                    return Ok(error_response(404, &format!("钱包不存在: {id}")));
                }
                drop(wallets);
                self.persist_wallets();
                Ok(ok_json(
                    serde_json::json!({"ok": true, "id": id, "action": "delete"}),
                ))
            }

            // —— GET /api/v1/blockchain/wallets/:id/balance —— 查余额（reqwest RPC）
            (HttpMethod::Get, ["api", "v1", "blockchain", "wallets", id, "balance"]) => {
                let snap = {
                    let wallets = self.wallets.lock().expect("wallets poisoned");
                    wallets.iter().find(|w| w.id == *id).cloned()
                };
                let Some(w) = snap else {
                    return Ok(error_response(404, &format!("钱包不存在: {id}")));
                };
                // 仅 EVM 链走 eth_getBalance；bitcoin/custom 直接降级 null。
                let (balance, note) = if w.chain_type == "evm" {
                    // rustify：reqwest POST eth_getBalance（原 curl 子进程移除）
                    let bal = fetch_balance_rpc("http://localhost:8545", &w.address).await;
                    let note = if bal.is_some() {
                        None
                    } else {
                        Some("RPC 节点不在线或无响应，余额降级为 null".to_string())
                    };
                    (bal, note)
                } else {
                    (
                        None,
                        Some(format!("{} 链暂不支持余额查询（降级 null）", w.chain_type)),
                    )
                };
                // 回填缓存余额
                {
                    let mut wallets = self.wallets.lock().expect("wallets poisoned");
                    if let Some(rec) = wallets.iter_mut().find(|x| x.id == *id) {
                        rec.balance = balance.clone();
                    }
                }
                self.persist_wallets();
                let mut resp = serde_json::json!({
                    "ok": true,
                    "id": id,
                    "address": w.address,
                    "masked_address": mask_address(&w.address),
                    "balance": balance,
                });
                if let Some(n) = note {
                    resp["note"] = serde_json::Value::String(n);
                }
                Ok(ok_json(resp))
            }

            // —— POST /api/v1/blockchain/wallets/:id/import —— 导入私钥（admin）
            (HttpMethod::Post, ["api", "v1", "blockchain", "wallets", id, "import"]) => {
                let body: ImportWalletBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析导入私钥请求体失败: {e}"))
                })?;
                if body.private_key.trim().is_empty() {
                    return Ok(error_response(400, "private_key 不可为空"));
                }
                let snap = {
                    let wallets = self.wallets.lock().expect("wallets poisoned");
                    wallets.iter().find(|w| w.id == *id).cloned()
                };
                let Some(w) = snap else {
                    return Ok(error_response(404, &format!("钱包不存在: {id}")));
                };
                // rustify：k256 纯 Rust 由私钥派生地址（原 python3 子进程移除）
                let priv_hex = body.private_key.trim().to_string();
                let address = match derive_address_from_private_key(&priv_hex, &w.chain_type) {
                    Ok(a) => a,
                    Err(e) => {
                        return Ok(error_response(
                            502,
                            &format!("私钥导入失败（解析/派生失败）: {e}"),
                        ))
                    }
                };
                let view = {
                    let mut wallets = self.wallets.lock().expect("wallets poisoned");
                    match wallets.iter_mut().find(|w| w.id == *id) {
                        Some(rec) => {
                            rec.address = address;
                            rec.private_key = Some(priv_hex);
                            rec.balance = None; // 地址变了，余额缓存作废
                            rec.view()
                        }
                        None => {
                            return Ok(error_response(404, &format!("钱包不存在: {id}")));
                        }
                    }
                };
                self.persist_wallets();
                Ok(ok_json(to_value(&view)?))
            }

            // —— POST /api/v1/blockchain/wallets/:id/sign —— 签名交易（admin，降级不 panic）
            (HttpMethod::Post, ["api", "v1", "blockchain", "wallets", id, "sign"]) => {
                let body: SignTxBody = serde_json::from_value(req.body).map_err(|e| {
                    ApiGatewayError::Internal(format!("解析签名交易请求体失败: {e}"))
                })?;
                if body.to.trim().is_empty() {
                    return Ok(error_response(400, "to 不可为空"));
                }
                let snap = {
                    let wallets = self.wallets.lock().expect("wallets poisoned");
                    wallets.iter().find(|w| w.id == *id).cloned()
                };
                let Some(w) = snap else {
                    return Ok(error_response(404, &format!("钱包不存在: {id}")));
                };
                let Some(priv_hex) = w.private_key.as_deref().filter(|s| !s.is_empty()) else {
                    return Ok(error_response(
                        400,
                        &format!("钱包 {} 未持有私钥（仅观察地址）", w.id),
                    ));
                };
                let value = body
                    .value
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "0".into());
                let data = body
                    .data
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| "0x".into());
                let chain_id = w.chain_id.to_string();
                let (ok, out) = Self::spawn_python(
                    WALLET_SIGN_SCRIPT,
                    &[
                        priv_hex,
                        body.to.trim(),
                        value.as_str(),
                        data.as_str(),
                        chain_id.as_str(),
                    ],
                )
                .await;
                let mut resp = serde_json::json!({
                    "ok": true,
                    "wallet_id": id,
                    "address": w.address,
                    "masked_address": mask_address(&w.address),
                });
                match parse_json_output(ok, &out) {
                    Some(v) => {
                        let degraded = v.get("signed").and_then(|s| s.as_bool()) == Some(false);
                        if degraded {
                            resp["warning"] = serde_json::Value::String(
                                "已降级为未签名交易（eth_account/web3 未安装）".into(),
                            );
                        }
                        resp["result"] = v;
                    }
                    None => {
                        resp["ok"] = serde_json::Value::Bool(false);
                        resp["warning"] = serde_json::Value::String(format!(
                            "签名调用失败（python3 不可用？降级未签名）: {out}"
                        ));
                    }
                }
                Ok(ok_json(resp))
            }

            // —— GET /api/v1/blockchain/wallets/:id/address —— 导出地址
            (HttpMethod::Get, ["api", "v1", "blockchain", "wallets", id, "address"]) => {
                let wallets = self.wallets.lock().expect("wallets poisoned");
                match wallets.iter().find(|w| w.id == *id) {
                    Some(w) => Ok(ok_json(serde_json::json!({
                        "id": w.id,
                        "address": w.address,
                        "masked_address": mask_address(&w.address),
                        "chain_type": w.chain_type,
                        "chain_id": w.chain_id,
                    }))),
                    None => Ok(error_response(404, &format!("钱包不存在: {id}"))),
                }
            }

            _ => Ok(error_response(404, &format!("未匹配路由: {}", req.path))),
        }
    }
}

// ----------------------------------------------------------------------------
// 内部辅助
// ----------------------------------------------------------------------------

fn spec(
    method: HttpMethod,
    path: &str,
    requires_auth: bool,
    required_roles: Vec<String>,
) -> RouteSpec {
    RouteSpec {
        method,
        path: path.to_string(),
        handler_component: "blockchain".to_string(),
        requires_auth,
        required_roles,
    }
}

fn ok_json(body: serde_json::Value) -> ApiResponse {
    ApiResponse {
        status: 200,
        body,
        headers: serde_json::json!({}),
    }
}

fn error_response(status: u16, msg: &str) -> ApiResponse {
    ApiResponse {
        status,
        body: serde_json::json!({"error": msg}),
        headers: serde_json::json!({}),
    }
}

fn to_value<T: Serialize>(v: &T) -> Result<serde_json::Value, ApiGatewayError> {
    serde_json::to_value(v).map_err(|e| ApiGatewayError::Internal(format!("响应序列化失败: {e}")))
}

fn path_segments(path: &str) -> Vec<&str> {
    let pure = path.split('?').next().unwrap_or(path);
    pure.split('/').filter(|s| !s.is_empty()).collect()
}

fn now_iso() -> String {
    use chrono::Local;
    Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

// ----------------------------------------------------------------------------
// demo 数据
// ----------------------------------------------------------------------------

fn demo_nodes() -> Vec<NodeInstance> {
    let mut dev_node = NodeInstance {
        id: "node-1".into(),
        name: "本地开发链".into(),
        chain: ChainConfig {
            chain_type: "dev".into(),
            chain_id: 1337,
            network: "dev".into(),
            name: "本地开发链".into(),
        },
        client: "anvil".into(),
        rpc_port: 8545,
        ws_port: Some(8546),
        data_dir: "/tank/blockchain/node-1".into(),
        sync_mode: "full".into(),
        status: "stopped".into(),
        enabled: true,
        created_at: "2026-08-08T09:00:00+08:00".into(),
        error: None,
        compose_yaml: None,
        start_cmd: None,
    };
    dev_node.compose_yaml = Some(build_node_compose(&dev_node));
    dev_node.start_cmd = Some(build_node_start_cmd(&dev_node));

    let mut sepolia_node = NodeInstance {
        id: "node-2".into(),
        name: "Sepolia 测试节点".into(),
        chain: ChainConfig {
            chain_type: "ethereum".into(),
            chain_id: 11_155_111,
            network: "sepolia".into(),
            name: "Sepolia 测试网".into(),
        },
        client: "geth".into(),
        rpc_port: 8555,
        ws_port: Some(8556),
        data_dir: "/tank/blockchain/node-2".into(),
        sync_mode: "snap".into(),
        status: "stopped".into(),
        enabled: true,
        created_at: "2026-08-08T09:30:00+08:00".into(),
        error: None,
        compose_yaml: None,
        start_cmd: None,
    };
    sepolia_node.compose_yaml = Some(build_node_compose(&sepolia_node));
    sepolia_node.start_cmd = Some(build_node_start_cmd(&sepolia_node));

    vec![dev_node, sepolia_node]
}

// ----------------------------------------------------------------------------
// 单元测试
// ----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn dev_node() -> NodeInstance {
        NodeInstance {
            id: "node-1".into(),
            name: "本地开发链".into(),
            chain: ChainConfig {
                chain_type: "dev".into(),
                chain_id: 1337,
                network: "dev".into(),
                name: "本地开发链".into(),
            },
            client: "anvil".into(),
            rpc_port: 8545,
            ws_port: Some(8546),
            data_dir: "/tank/blockchain/node-1".into(),
            sync_mode: "full".into(),
            status: "stopped".into(),
            enabled: true,
            created_at: "2026-08-08T09:00:00+08:00".into(),
            error: None,
            compose_yaml: None,
            start_cmd: None,
        }
    }

    fn explorer_for(node: &NodeInstance) -> ExplorerInstance {
        ExplorerInstance {
            id: "explorer-1".into(),
            name: "本地浏览器".into(),
            node_id: node.id.clone(),
            web_port: 4000,
            db_port: Some(5432),
            status: "stopped".into(),
            url: Some("http://localhost:4000".into()),
            created_at: "2026-08-08T09:00:00+08:00".into(),
            compose_yaml: None,
            error: None,
        }
    }

    // ---- build_node_compose 测试 ----

    #[test]
    fn build_node_compose_geth_contains_http_flag() {
        let mut n = dev_node();
        n.client = "geth".into();
        n.chain = ChainConfig {
            chain_type: "ethereum".into(),
            chain_id: 1,
            network: "mainnet".into(),
            name: "Ethereum 主网".into(),
        };
        let y = build_node_compose(&n);
        assert!(y.contains("--http"), "geth compose 应含 --http: {y}");
        assert!(y.contains("ghcr.io/ethereum/client-go"), "geth image: {y}");
        assert!(y.contains("--mainnet"), "geth 网络 flag: {y}");
    }

    #[test]
    fn build_node_compose_reth_contains_reth_node() {
        let mut n = dev_node();
        n.client = "reth".into();
        n.chain = ChainConfig {
            chain_type: "ethereum".into(),
            chain_id: 1,
            network: "mainnet".into(),
            name: "Ethereum 主网".into(),
        };
        let y = build_node_compose(&n);
        assert!(y.contains("node"), "reth compose 应含 node 子命令: {y}");
        assert!(y.contains("--chain=mainnet"), "reth chain flag: {y}");
    }

    #[test]
    fn build_node_compose_ganache_contains_port_flag() {
        let mut n = dev_node();
        n.client = "ganache".into();
        let y = build_node_compose(&n);
        assert!(y.contains("-p 8545"), "ganache compose 应含 -p 8545: {y}");
        assert!(y.contains("trufflesuite/ganache"), "ganache image: {y}");
    }

    #[test]
    fn build_node_compose_anvil_contains_chain_id_flag() {
        let mut n = dev_node();
        n.client = "anvil".into();
        let y = build_node_compose(&n);
        assert!(
            y.contains("--chain-id 1337"),
            "anvil compose 应含 --chain-id: {y}"
        );
        assert!(y.contains("foundry-rs/foundry"), "anvil image: {y}");
    }

    #[test]
    fn build_node_compose_erigon_contains_chain_flag() {
        let mut n = dev_node();
        n.client = "erigon".into();
        n.chain = ChainConfig {
            chain_type: "ethereum".into(),
            chain_id: 1,
            network: "mainnet".into(),
            name: "Ethereum 主网".into(),
        };
        let y = build_node_compose(&n);
        assert!(y.contains("--chain=mainnet"), "erigon chain flag: {y}");
        assert!(y.contains("thorax/erigon"), "erigon image: {y}");
    }

    // ---- build_explorer_compose 测试 ----

    #[test]
    fn build_explorer_compose_points_to_node_rpc_port() {
        let n = dev_node();
        let e = explorer_for(&n);
        let y = build_explorer_compose(&e, &n);
        assert!(
            y.contains(&format!(
                "ETHEREUM_JSONRPC_HTTP_URL: http://host.docker.internal:{}",
                n.rpc_port
            )),
            "explorer compose 应指向节点 RPC 端口: {y}"
        );
        let ws = n.ws_port.unwrap_or(8546);
        assert!(
            y.contains(&format!(
                "ETHEREUM_JSONRPC_WS_URL: ws://host.docker.internal:{ws}"
            )),
            "explorer compose 应指向节点 WS 端口: {y}"
        );
    }

    #[test]
    fn build_explorer_compose_contains_postgres_service() {
        let n = dev_node();
        let e = explorer_for(&n);
        let y = build_explorer_compose(&e, &n);
        assert!(y.contains("postgres:15"), "应含 postgres image: {y}");
        assert!(y.contains("DATABASE_URL"), "应含 DATABASE_URL: {y}");
        assert!(
            y.contains("blockscout:latest"),
            "应含 blockscout image: {y}"
        );
    }

    #[test]
    fn build_explorer_compose_maps_web_port() {
        let n = dev_node();
        let mut e = explorer_for(&n);
        e.web_port = 4321;
        let y = build_explorer_compose(&e, &n);
        assert!(
            y.contains("\"4321:4000\""),
            "应映射 web 端口 4321:4000: {y}"
        );
    }

    // ---- build_node_start_cmd 测试 ----

    #[test]
    fn build_node_start_cmd_contains_docker_compose_up() {
        let n = dev_node();
        let c = build_node_start_cmd(&n);
        assert!(
            c.contains("docker compose up -d"),
            "应含 docker compose up -d: {c}"
        );
        assert!(c.contains(&n.data_dir), "应 cd 到数据目录: {c}");
    }

    // ---- chain-presets 测试 ----

    #[test]
    fn chain_presets_returns_seven_entries() {
        let p = chain_presets();
        assert_eq!(p.len(), 7, "应返回 7 条链预设");
        // 关键链存在
        assert!(p.iter().any(|x| x.network == "mainnet"));
        assert!(p.iter().any(|x| x.network == "sepolia"));
        assert!(p.iter().any(|x| x.network == "dev"));
        assert!(p.iter().any(|x| x.network == "optimism"));
        assert!(p.iter().any(|x| x.network == "arbitrum"));
        assert!(p.iter().any(|x| x.network == "base"));
        assert!(p.iter().any(|x| x.network == "custom"));
    }

    // ---- 路由声明测试 ----

    #[tokio::test]
    async fn routes_declares_all_endpoints_under_blockchain() {
        let h = BlockchainRouteHandler::new();
        let routes = h.routes().await;
        // 6 节点 + 4 浏览器 + 3 预设/统计/客户端 + 7 钱包 + 9 chain-nodes = 29
        assert_eq!(routes.len(), 29, "应有 29 条路由: {routes:?}");
        assert!(
            routes.iter().all(|r| r.handler_component == "blockchain"),
            "全部归属 blockchain 组件"
        );
    }

    // ---- 钱包纯函数测试 ----

    #[test]
    fn mask_address_formats_long_addresses() {
        assert_eq!(
            mask_address("0x1234567890abcdef1234567890abcdef12345678"),
            "0x1234...5678"
        );
        // 短地址原样返回
        assert_eq!(mask_address("0xabc"), "0xabc");
        assert_eq!(mask_address(""), "");
    }

    #[test]
    fn build_balance_payload_contains_get_balance_and_address() {
        let p = build_balance_payload("0xabc123");
        assert_eq!(p["method"].as_str(), Some("eth_getBalance"));
        assert_eq!(p["params"][0].as_str(), Some("0xabc123"));
        assert_eq!(p["params"][1].as_str(), Some("latest"));
        assert_eq!(p["jsonrpc"].as_str(), Some("2.0"));
    }

    #[test]
    fn balance_and_keygen_parsers_degrade() {
        // 节点离线 / 非 JSON 输出 → None（不 panic）
        assert!(parse_balance_output(false, "curl: (7) Failed to connect").is_none());
        assert!(parse_balance_output(true, "not json").is_none());
        assert_eq!(
            parse_balance_output(true, r#"{"jsonrpc":"2.0","result":"0x1bc16d674ec80000"}"#),
            Some("0x1bc16d674ec80000".to_string())
        );
    }

    #[test]
    fn derive_evm_address_matches_known_vector() {
        // 私钥 0x…01 的 EVM 地址公开测试向量（与原 python keccak 脚本逐字节一致）
        let addr = derive_address_from_private_key(
            "0x0000000000000000000000000000000000000000000000000000000000000001",
            "evm",
        )
        .expect("合法私钥应派生成功");
        assert_eq!(addr, "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf");
        // 非法输入 → Err（不 panic）
        assert!(derive_address_from_private_key("zz", "evm").is_err());
        assert!(derive_address_from_private_key("0x1234", "evm").is_err());
    }

    #[test]
    fn generate_wallet_keypair_shapes_and_reproducible() {
        let (key, addr) = generate_wallet_keypair("evm").expect("生成应成功");
        assert!(
            key.starts_with("0x") && key.len() == 66,
            "私钥应 0x+64hex: {key}"
        );
        assert!(
            addr.starts_with("0x") && addr.len() == 42,
            "地址应 0x+40hex: {addr}"
        );
        // 同私钥派生地址应可复现
        let again = derive_address_from_private_key(&key, "evm").expect("派生应成功");
        assert_eq!(addr, again);
    }

    #[test]
    fn wallet_record_view_never_leaks_private_key() {
        let rec = WalletRecord {
            id: "wallet-1".into(),
            name: "主钱包".into(),
            chain_type: "evm".into(),
            chain_id: 1,
            address: "0x1234567890abcdef1234567890abcdef12345678".into(),
            private_key: Some("0xdeadbeef".into()),
            balance: None,
            created_at: "2026-08-13T09:00:00+08:00".into(),
        };
        let view = rec.view();
        assert!(view.has_private_key);
        let s = serde_json::to_string(&view).expect("序列化");
        assert!(!s.contains("deadbeef"), "私钥不得出现在 API 序列化: {s}");
        assert!(
            !s.contains("\"private_key\""),
            "不得含 private_key 字段: {s}"
        );
        assert!(s.contains("has_private_key"));
    }

    // ---- 钱包路由测试 ----

    fn get_req(path: &str) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Get,
            path: path.into(),
            headers: serde_json::json!({}),
            body: serde_json::Value::Null,
            auth: None,
        }
    }

    // ---- blockchain_nodes 子模块委托（chain-nodes 前缀经父 handler 分发）----

    #[tokio::test]
    async fn chain_nodes_prefix_delegates_to_submodule() {
        let h = BlockchainRouteHandler::with_empty();
        // presets 经父 handler 前缀委托到达子模块（含 query 透传）
        let resp = h
            .handle(get_req("/api/v1/blockchain/chain-nodes/presets"))
            .await
            .expect("handle 不应失败");
        assert_eq!(resp.status, 200, "presets 应 200: {}", resp.body);
        let presets = resp.body["presets"].as_array().expect("presets 数组");
        assert_eq!(presets.len(), 6, "eth 3 + btc 3 网络");
        assert!(
            resp.body["default_data_root"].as_str().is_some_and(|s| !s.is_empty()),
            "应带默认数据根目录"
        );
        // 列表端点同前缀可达（空列表）
        let list = h
            .handle(get_req("/api/v1/blockchain/chain-nodes"))
            .await
            .expect("handle 不应失败");
        assert_eq!(list.status, 200);
        assert_eq!(list.body.as_array().map(Vec::len), Some(0));
        // space-check 的 query 串透传（路径带 ? 不影响段匹配）
        let sc = h
            .handle(get_req(
                "/api/v1/blockchain/chain-nodes/space-check?kind=bitcoin&network=regtest&mode=fast&data_dir=/tmp/nexos-bcn-delegate",
            ))
            .await
            .expect("handle 不应失败");
        assert_eq!(sc.status, 200, "space-check 应 200: {}", sc.body);
    }

    #[tokio::test]
    async fn create_wallet_spawns_python_and_masks_key() {
        let h = BlockchainRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/blockchain/wallets",
                serde_json::json!({"name": "主钱包", "chain_type": "evm"}),
            ))
            .await
            .expect("handle 不应失败");
        assert_eq!(resp.status, 201, "创建钱包应 201");
        let addr = resp
            .body
            .get("address")
            .and_then(|v| v.as_str())
            .expect("address 应存在");
        assert!(addr.starts_with("0x"), "地址应 0x 开头: {addr}");
        assert_eq!(addr.len(), 42, "EVM 地址应 40 hex: {addr}");
        assert_eq!(
            resp.body.get("has_private_key").and_then(|v| v.as_bool()),
            Some(true),
            "生成钱包应持有私钥"
        );
        let raw = serde_json::to_string(&resp.body).unwrap();
        assert!(!raw.contains("\"private_key\""), "响应不得泄露私钥: {raw}");
    }

    #[tokio::test]
    async fn create_wallet_rejects_empty_name() {
        let h = BlockchainRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/blockchain/wallets",
                serde_json::json!({"name": ""}),
            ))
            .await
            .expect("handle 不应失败");
        assert_eq!(resp.status, 400, "空 name 应 400");
    }

    #[tokio::test]
    async fn wallet_list_delete_roundtrip() {
        let h = BlockchainRouteHandler::with_empty();
        let created = h
            .handle(post_req(
                "/api/v1/blockchain/wallets",
                serde_json::json!({"name": "临时钱包"}),
            ))
            .await
            .expect("handle 不应失败");
        let id = created
            .body
            .get("id")
            .and_then(|v| v.as_str())
            .expect("id 应存在")
            .to_string();

        // 列表含新钱包且无私钥
        let list = h
            .handle(get_req("/api/v1/blockchain/wallets"))
            .await
            .unwrap();
        assert_eq!(list.status, 200);
        let arr = list.body.as_array().expect("列表应为数组");
        assert_eq!(arr.len(), 1);
        assert!(!list.body.to_string().contains("\"private_key\""));

        // 删除 → 404 后续查询
        let del = h
            .handle(ApiRequest {
                method: HttpMethod::Delete,
                path: format!("/api/v1/blockchain/wallets/{id}"),
                headers: serde_json::json!({}),
                body: serde_json::Value::Null,
                auth: None,
            })
            .await
            .unwrap();
        assert_eq!(del.status, 200);
        let gone = h
            .handle(get_req(&format!("/api/v1/blockchain/wallets/{id}/address")))
            .await
            .unwrap();
        assert_eq!(gone.status, 404, "删除后应 404");

        // 再删 → 404
        let del2 = h
            .handle(ApiRequest {
                method: HttpMethod::Delete,
                path: format!("/api/v1/blockchain/wallets/{id}"),
                headers: serde_json::json!({}),
                body: serde_json::Value::Null,
                auth: None,
            })
            .await
            .unwrap();
        assert_eq!(del2.status, 404);
    }

    #[tokio::test]
    async fn wallet_balance_missing_404_and_existing_degrades() {
        let h = BlockchainRouteHandler::with_empty();
        let resp = h
            .handle(get_req("/api/v1/blockchain/wallets/wallet-999/balance"))
            .await
            .unwrap();
        assert_eq!(resp.status, 404);

        // 创建后查余额：RPC 节点不在线 → 200 + balance null（降级不 panic）
        let created = h
            .handle(post_req(
                "/api/v1/blockchain/wallets",
                serde_json::json!({"name": "余额钱包", "chain_type": "evm"}),
            ))
            .await
            .unwrap();
        let id = created
            .body
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();
        let bal = h
            .handle(get_req(&format!("/api/v1/blockchain/wallets/{id}/balance")))
            .await
            .unwrap();
        assert_eq!(bal.status, 200, "余额查询不 panic: {}", bal.body);
        assert!(
            bal.body.get("balance").is_some(),
            "应含 balance 字段（可为 null）"
        );
    }

    #[tokio::test]
    async fn wallet_sign_without_key_is_400_and_import_derives_address() {
        let h = BlockchainRouteHandler::with_empty();
        // 先造一个"无密钥"的钱包：直接注入 WalletRecord（绕过生成）。
        {
            let mut wallets = h.wallets.lock().expect("wallets poisoned");
            wallets.push(WalletRecord {
                id: "wallet-777".into(),
                name: "观察钱包".into(),
                chain_type: "evm".into(),
                chain_id: 1,
                address: "0x0000000000000000000000000000000000000000".into(),
                private_key: None,
                balance: None,
                created_at: "2026-08-13T09:00:00+08:00".into(),
            });
        }
        // 未持私钥签名 → 400
        let sign = h
            .handle(post_req(
                "/api/v1/blockchain/wallets/wallet-777/sign",
                serde_json::json!({"to": "0x1111111111111111111111111111111111111111", "value": "1"}),
            ))
            .await
            .unwrap();
        assert_eq!(sign.status, 400, "无私钥签名应 400");

        // 导入已知私钥 0x...01 → 派生地址应为公开测试向量
        let import = h
            .handle(post_req(
                "/api/v1/blockchain/wallets/wallet-777/import",
                serde_json::json!({"private_key": "0x0000000000000000000000000000000000000000000000000000000000000001"}),
            ))
            .await
            .unwrap();
        assert_eq!(import.status, 200, "导入应 200: {}", import.body);
        assert_eq!(
            import.body.get("address").and_then(|v| v.as_str()),
            Some("0x7e5f4552091a69125d5dfcb7b8c2659029395bdf"),
            "keccak/secp256k1 派生地址应为已知向量（真实 EVM：keccak256(64B 公钥)[12..]）"
        );
        assert_eq!(
            import.body.get("has_private_key").and_then(|v| v.as_bool()),
            Some(true)
        );
        let raw = serde_json::to_string(&import.body).unwrap();
        assert!(!raw.contains("0000000000000000000000000000000000000000000000000000000000000001"));

        // 持钥签名 → 200（eth_account 缺失时降级 signed:false）
        let sign2 = h
            .handle(post_req(
                "/api/v1/blockchain/wallets/wallet-777/sign",
                serde_json::json!({"to": "0x1111111111111111111111111111111111111111", "value": "1"}),
            ))
            .await
            .unwrap();
        assert_eq!(sign2.status, 200, "签名应 200: {}", sign2.body);
        assert!(sign2.body.get("result").is_some(), "应含签名/降级结果");
        let raw = serde_json::to_string(&sign2.body).unwrap();
        assert!(
            !raw.contains("0x0000000000000000000000000000000000000000000000000000000000000001"),
            "签名响应不得回显私钥"
        );
    }

    #[tokio::test]
    async fn wallet_address_export_is_masked() {
        let h = BlockchainRouteHandler::with_empty();
        let created = h
            .handle(post_req(
                "/api/v1/blockchain/wallets",
                serde_json::json!({"name": "地址钱包"}),
            ))
            .await
            .unwrap();
        let id = created
            .body
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();
        let resp = h
            .handle(get_req(&format!("/api/v1/blockchain/wallets/{id}/address")))
            .await
            .unwrap();
        assert_eq!(resp.status, 200);
        let masked = resp
            .body
            .get("masked_address")
            .and_then(|v| v.as_str())
            .unwrap();
        assert!(masked.contains("..."), "脱敏地址应含省略号: {masked}");
    }

    // ---- POST 创建节点测试 ----

    fn post_req(path: &str, body: serde_json::Value) -> ApiRequest {
        ApiRequest {
            method: HttpMethod::Post,
            path: path.into(),
            headers: serde_json::json!({}),
            body,
            auth: None,
        }
    }

    #[tokio::test]
    async fn create_node_yields_non_empty_compose_yaml() {
        let h = BlockchainRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/blockchain/nodes",
                serde_json::json!({
                    "name": "测试节点",
                    "chain_type": "ethereum",
                    "network": "mainnet",
                    "client": "geth",
                    "rpc_port": 8645,
                    "ws_port": 8646,
                }),
            ))
            .await
            .expect("handle 不应失败");
        assert_eq!(resp.status, 201, "创建应返回 201");
        let compose = resp
            .body
            .get("compose_yaml")
            .and_then(|v| v.as_str())
            .expect("compose_yaml 应存在");
        assert!(!compose.is_empty(), "compose_yaml 非空: {compose}");
        assert!(compose.contains("--http"), "compose 应含 --http");
        let start_cmd = resp
            .body
            .get("start_cmd")
            .and_then(|v| v.as_str())
            .expect("start_cmd 应存在");
        assert!(start_cmd.contains("docker compose up -d"));
    }

    #[tokio::test]
    async fn create_node_rejects_empty_name() {
        let h = BlockchainRouteHandler::with_empty();
        let resp = h
            .handle(post_req(
                "/api/v1/blockchain/nodes",
                serde_json::json!({"name": ""}),
            ))
            .await
            .expect("handle 不应失败");
        assert_eq!(resp.status, 400, "空 name 应 400");
    }

    #[tokio::test]
    async fn create_explorer_yields_compose_pointing_to_node() {
        let h = BlockchainRouteHandler::new();
        // demo node-1 已存在
        let resp = h
            .handle(post_req(
                "/api/v1/blockchain/explorers",
                serde_json::json!({
                    "name": "本地浏览器",
                    "node_id": "node-1",
                    "web_port": 4000,
                }),
            ))
            .await
            .expect("handle 不应失败");
        assert_eq!(resp.status, 201, "创建浏览器应 201");
        let compose = resp
            .body
            .get("compose_yaml")
            .and_then(|v| v.as_str())
            .expect("compose_yaml 应存在");
        assert!(
            compose.contains("host.docker.internal:8545"),
            "应指向节点端口"
        );
    }

    #[tokio::test]
    async fn stats_reports_node_and_explorer_counts() {
        let h = BlockchainRouteHandler::new();
        let resp = h
            .handle(ApiRequest {
                method: HttpMethod::Get,
                path: "/api/v1/blockchain/stats".into(),
                headers: serde_json::json!({}),
                body: serde_json::Value::Null,
                auth: None,
            })
            .await
            .expect("handle 不应失败");
        assert_eq!(resp.status, 200);
        let total = resp.body.get("nodes_total").and_then(|v| v.as_u64());
        assert_eq!(total, Some(2), "应有 2 个 demo 节点");
        let chains = resp.body.get("supported_chains").and_then(|v| v.as_u64());
        assert_eq!(chains, Some(7), "应支持 7 条链");
    }
}
