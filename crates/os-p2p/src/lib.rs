//! os-p2p —— NexOS 点对点组网层（Swarm 同款全分布式 Kademlia，P1+P2a+P2b）。
//!
//! 设计定稿 docs/NEXOS_P2P_NETWORK_DESIGN.md（用户 2026-08-20）：**全分布式
//! Kademlia**——不设中心注册表，节点发现与路由完全经 DHT walk；公网节点
//! （`NEXOS_P2P_PUBLIC=1`）仅承担 bootstrap 冷启动与 NAT 中继服务，是"服务节点"
//! 不是"中心"。NexOS 定位 = 独立个体操作系统，每个实例天然是一个对等节点。
//!
//! P2b 增补：**密钥持久化**（[`bootstrap::load_or_create_identity`] +
//! `NEXOS_P2P_KEY_FILE`）——CLI 与库共用一份 secp256k1 私钥文件（原子写 0600），
//! 锚点/节点重启 NodeID 稳定（修身份漂移）。
//!
//! # 身份与地址（EVM 同源，零新增身份体系）
//!
//! - **NodeID** = secp256k1 压缩公钥（`0x` + 66 hex）——直接复用
//!   `os_common::chain_auth` 的解析/验签（IM/NexHub 的链上身份就是节点身份）。
//! - **OverlayAddr** = `keccak256(未压缩公钥[1..])[12..]` 的 20 字节——与 EVM
//!   地址派生同源（160-bit 地址空间，邻域阶 0..=159，共 160 个 proximity bin）。
//!
//! # 协议帧（transport + crypto，P2a 加密链路）
//!
//! tokio TCP + 4 字节大端长度前缀 + JSON 信封 `{type, version, src, dst,
//! payload, ttl, hops}`。连接建立即互发 Hello（含 ECDH 临时公钥），随后双向
//! **nonce 挑战-签名**（签名覆盖 nonce+双方临时公钥——防 MITM），ECDH 派生
//! 共享密钥后**所有帧 AES-256-GCM 加密**（长度前缀 + 12B nonce + 密文）。
//! 帧带 version 字段且**无明文回落**——同版本才可互联。
//!
//! # 连接阶梯（P2a：观测端点八卦 + TCP 打洞 + 中继兜底）
//!
//! ```text
//!  ① LAN 直连（mDNS 种子发现）  ② 公网直连（underlay）
//!  ③ TCP 打洞（观测端点 + 同时打开）  ④ 中继兜底（任意可达节点）
//!  Handle::connect(node_id) → ConnectPath::{Direct, Punched, Relayed}
//! ```
//!
//! # 模块（设计 §3）
//!
//! - [`identity`]：NodeID / OverlayAddr / XOR 距离 / 邻域阶（纯函数）。
//! - [`crypto`]：ECDH 临时密钥 + AES-256-GCM 会话密码（P2a 链路加密）。
//! - [`transport`]：帧编解码 + 长度前缀 TCP 读写 + 认证密钥交换握手。
//! - [`kad`]：k-buckets（k=16，按 proximity order 分桶）+ FINDNODE 迭代查询收敛。
//! - [`endpoints`]：观测端点地址簿（地址交换所）+ 随 NODES 的八卦扩散 + TTL。
//! - [`meta`]：节点元数据组件——集群节点注册表 + 专用心跳检测引擎 + 元数据
//!   交互 + 健康排名 + 持久化（本 crate 唯一的节点存活判定账本）。
//! - [`transfer`]：P2P 传输组件——文件清单（发布/分块 sha256）+ query/offer
//!   请求-应答 + 分块拉取引擎（背压/校验/重试/断点续传），经 overlay 消息
//!   通道分发（NAT 后节点互传不依赖公网 IP）。
//! - [`punch`]：TCP 打洞（PUNCH1/PUNCH2 端点交换 + 同时打开）+ 连接阶梯。
//! - [`relay`]：NAT 可达性记录 `{dst → 经 relay_id}` + SEND 路由决策 +
//!   store-and-forward 离线信箱（上限 100 条/节点）。
//! - [`bootstrap`]：mDNS LAN 种子（`_nexos-p2p._tcp`）+ `NEXOS_P2P_BOOTSTRAP`
//!   冷启动 walk + 引导连接保活 + `NEXOS_P2P_KEY_FILE` 密钥持久化（P2b）。
//! - [`api`]：[`P2pNode::spawn`] → [`Handle`]（send / connect / on_msg /
//!   peers / known_endpoints / ladder_stats / identity_conflicts——上层服务接入面）。
//!
//! # 身份账本外移（2026-08-25，os-identity 组件）
//!
//! 指纹（NodeID↔地址）的**账本与对比已抽到独立 crate `os-identity`**（用户
//! 定调：「指纹对比单独做一个组件，不要集成在 p2p 里面」）——本 crate 回归
//! 纯传输层：握手/指纹探测只产出**事实事件**写进账本（`P2pConfig::
//! identity_ledger` 注入共享实例，None 时本地内存自建），「记谁/信谁/地址
//! 属于谁」的策略与 `identity_conflicts` 记账、`owns_addr` 对比、持久化全部
//! 由 os-identity 承担。架构见 `docs/IDENTITY_COMPONENT.md`。
//!
//! # 依赖面（P1 裁决 + P2a 扩展）
//!
//! `os-common`（chain_auth 密钥学）+ tokio/serde/k256(ecdh)/aes-gcm/socket2/
//! mdns-sd 栈；**不依赖 os-discover**（mDNS 用 mdns-sd 直连，独立于其联邦
//! 状态机），也不依赖 os-api。
//!
//! # 快速上手
//!
//! ```
//! use os_p2p::{P2pConfig, P2pNode};
//!
//! # #[tokio::main(flavor = "current_thread")] async fn main() {
//! let handle = P2pNode::spawn(P2pConfig {
//!     listen: "127.0.0.1:0".parse().unwrap(),
//!     bootstrap: vec!["203.0.113.10:7070".parse().unwrap()],
//!     public: true, // 公网节点：承担 bootstrap / relay 服务职责
//!     ..P2pConfig::default()
//! })
//! .unwrap();
//! handle.send(&handle.self_id().clone(), serde_json::json!({"ping": 1}));
//! # }
//! ```
//!
//! 独立节点载体（cloud 锚点/交换所部署）：`cargo run -p os-p2p --bin p2p-node`。
//!
//! # 测试
//!
//! `cargo test -p os-p2p`：纯函数单测（距离/邻域阶/桶选择/帧编解码/ECDH 握手
//! 正反/加密帧/端点簿/打洞计划）+ 单机多实例集成测试（随机端口组网：收敛 /
//! 互通 / kill 剔除自愈 / 中途加入 / 离线消息 / 观测端点八卦 / loopback 打洞
//! （ConnectPath=Punched）/ 打洞失败落中继 / 阶梯短路 / mDNS 降级）+ CLI 冒烟。

pub mod api;
pub mod bootstrap;
pub mod crypto;
pub mod endpoints;
pub mod identity;
pub mod kad;
pub mod meta;
pub mod punch;
pub mod relay;
pub mod transfer;
pub mod transport;

pub use api::{Handle, P2pConfig, P2pMsg, P2pNode, PeerInfo, Timing};
pub use bootstrap::{
    config_from_env, default_key_file, load_or_create_identity, parse_bootstrap_list, parse_listen,
    truthy, ENV_BOOTSTRAP, ENV_EXIT_OFFER, ENV_IDENTITY_FILE, ENV_KEY_FILE, ENV_LISTEN, ENV_MDNS,
    ENV_META_FILE, ENV_NAME, ENV_PUBLIC, MDNS_SERVICE_TYPE, P2P_PORT_DEFAULT,
};
pub use crypto::{SessionCipher, AEAD_NONCE_LEN, AEAD_TAG_LEN};
pub use endpoints::{EndpointBook, EndpointEntry, EndpointGossip, ENDPOINTS_GOSSIP_LIMIT};
pub use identity::{NodeId, NodeIdentity, OverlayAddr};
pub use kad::{BucketStat, KBuckets, NodeInfo};
// 身份冲突观测结构迁至 os-identity（形状不变）——本 crate 转发导出，既有消费方
//（os-api identity-conflicts 端点等）无感。
pub use meta::{MetaAddr, MetaDigestEntry, MetaSource, MetaState};
pub use meta::{
    NodeMetaEntry, META_ADDRS_CAP, META_DIGEST_FRESH_SECS, META_DIGEST_MAX_BYTES,
    META_GOSSIP_EVERY_TICKS, META_MAX_CONSEC_FAIL, META_REVIVE_SCORE, META_SCORE_FAIL_STEP,
    META_SCORE_MAX, META_SCORE_START, META_SCORE_SUCCESS_STEP,
};
pub use os_identity::{IdentityConflict, SharedLedger};
pub use punch::{ConnectError, ConnectPath, LadderStats, PUNCH_ATTEMPTS, PUNCH_RDV_DELAY};
pub use relay::{next_hop, NextHop, RelayState, MAILBOX_LIMIT_PER_NODE};
pub use transfer::{
    build_manifest, chunk_count, chunk_len, chunk_offset, verify_whole_file, ProgressState,
    RegistryEntry, TaskPhase, TransferConfig, TransferManifest, TransferRegistry, TransferService,
    TransferStats, TransferTaskView, CHUNK_RETRIES, CHUNK_SIZE, CHUNK_TIMEOUT, KIND_CHUNK_DATA,
    KIND_CHUNK_REQ, KIND_ERROR, KIND_OFFER, KIND_QUERY, MANIFEST_MAX_CHUNKS, MAX_INFLIGHT_CHUNKS,
    QUERY_WINDOW,
};
pub use transport::{Frame, FrameKind, DEFAULT_TTL, MAX_FRAME_LEN, PROTOCOL_VERSION};

/// os-p2p 统一错误（传输 / 握手 / 加密 / 帧 / 路由）。
#[derive(Debug, thiserror::Error)]
pub enum P2pError {
    /// TCP 层 I/O 错误（连接断开 / 拨号失败 / 读写超时底层原因）。
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// JSON 帧编解码失败（对端与本版本协议不兼容，或链路损坏）。
    #[error("frame json: {0}")]
    FrameJson(#[from] serde_json::Error),
    /// 握手失败：对端未在挑战-签名中证明持有 NodeID 私钥（或流程/格式非法，
    /// 或 ECDH 临时公钥被中途替换——转录本验签失败）。
    #[error("handshake with {peer}: {reason}")]
    Handshake {
        /// 对端 NodeID（Hello 已交换时；否则短 hex 占位）。
        peer: String,
        /// 失败原因。
        reason: String,
    },
    /// 协议版本不一致（**无明文回落**——异版本直接拒连）。
    #[error("protocol version mismatch: ours={ours}, theirs={theirs}")]
    VersionMismatch {
        /// 本端协议版本。
        ours: u32,
        /// 对端协议版本（0 = 缺失，旧版本帧）。
        theirs: u32,
    },
    /// 链路加密错误（AES-256-GCM 标签不符 = 篡改/密钥不配，或密钥材料非法）。
    #[error("crypto: {0}")]
    Crypto(String),
    /// 帧超过 [`MAX_FRAME_LEN`] 上限（对端异常或恶意，直接断连）。
    #[error("frame too large: {0} bytes (max {MAX_FRAME_LEN})")]
    FrameTooLarge(usize),
}

/// 通用 Result 别名。
pub type Result<T> = std::result::Result<T, P2pError>;

/// 日志/摘要用短 ID：`0x1234…abcd`（前 4 + 后 4 hex）。
pub(crate) fn short_hex(s: &str) -> String {
    let n = s.len();
    if n <= 10 {
        s.to_string()
    } else {
        format!("{}…{}", &s[..6], &s[n - 4..])
    }
}
