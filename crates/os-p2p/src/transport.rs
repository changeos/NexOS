//! 节点间传输——设计 §3「transport」：长度前缀 JSON 帧 + 认证密钥交换握手。
//!
//! P1：tokio TCP + 手写帧 + **每连接双向 nonce 挑战**（对端必须证明持有其声称
//! NodeID 的 secp256k1 私钥）。P2a 在同一握手上叠加 **ECDHE**：Hello 互发临时
//! 公钥，挑战签名覆盖「nonce + 双方临时公钥」（[`crate::crypto::ecdh_transcript`]
//! ——中途替换任一临时公钥即验签失败，防 MITM），派生 AES-256-GCM 会话密钥后
//! **所有数据帧加密**。
//!
//! # 线上帧格式
//!
//! 握手帧（明文，交换身份与密钥材料）：
//!
//! ```text
//! ┌────────────────┬────────────────────────────────────────────┐
//! │ u32 BE length  │  明文 JSON 信封（hello/challenge/response）│
//! └────────────────┴────────────────────────────────────────────┘
//! ```
//!
//! 数据帧（握手完成后全部加密）：
//!
//! ```text
//! ┌────────────────┬───────────────┬──────────────────────────────┐
//! │ u32 BE length  │ nonce (12B)   │ AES-256-GCM 密文 ‖ 标签(16B) │
//! └────────────────┴───────────────┴──────────────────────────────┘
//! 密文 = 信封 JSON；length = 12 + len(密文+标签) ≤ MAX_FRAME_LEN（4 MiB 沿用）
//! ```
//!
//! 信封 = `{"type":<kind>, "version":<u32>, "src":"0x<66hex>",
//! "dst":"0x<66hex>"|null, "ttl":<u8>, "hops":<u8>, "payload":{...}}`。
//!
//! - `type`：[`FrameKind`]（hello / auth_challenge / auth_response / ping / pong /
//!   findnode / nodes / send / relay_announce / punch1 / punch2 / meta_gossip）。
//! - `version`：协议版本（[`PROTOCOL_VERSION`]）。**无明文回落**——Hello 版本
//!   不一致立即拒连（旧节点/异版本节点无法接入，见 [`P2pError::VersionMismatch`]）。
//! - `src`/`dst`：发送方 / 最终目标 NodeID。控制帧的 dst 通常是对端（或握手
//!   前为 null）；**send 帧的 dst 是最终接收者**——中继转发时保持不变。
//!   punch1/punch2 的 dst 也是最终对端（共同中介节点按直连转交，见 punch.rs）。
//! - `ttl`/`hops`：防环与跳数统计。**每经一个中继转发 ttl-1 / hops+1**；ttl 减
//!   到 0 的帧被丢弃（[`Frame::hopped`]）。默认 [`DEFAULT_TTL`] = 16。
//! - `payload`：按 kind 约定的 JSON 对象（见 [`Frame`] 各构造器）。
//!
//! # 握手（双向认证 + ECDH 密钥协商，5s 超时）
//!
//! 双方连接后按同一脚本对称执行（无序等待，天然无死锁）：
//!
//! ```text
//!   A ── hello{version, id, underlay, public, eph_A} ──▶ B   （双方都先发）
//!   A ◀────────────────────────────────────────────── hello{…, eph_B}
//!   A ── auth_challenge{req_id, nonce_A} ────────────▶ B
//!   A ◀── auth_challenge{req_id, nonce_B} ──────────── B
//!   A ── auth_response{sig(nonce_B ‖ eph_A ‖ eph_B)} ─▶ B
//!   A ◀── auth_response{sig(nonce_A ‖ eph_B ‖ eph_A)} ─ B    （chain_auth 验签）
//!   A ◀══════ 双方各自 ECDH 派生同一会话密钥 → 数据帧全部 AES-256-GCM ══════▶ B
//! ```

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::crypto::{ecdh_transcript, EphemeralKey, SessionCipher};
use crate::endpoints::EndpointGossip;
use crate::identity::{NodeId, NodeIdentity};
use crate::{P2pError, Result};

/// 单帧 JSON 信封上限（4 MiB——应用 payload 的防呆线，超限断连；加密帧的
/// length 含 nonce+标签开销，同限沿用）。
pub const MAX_FRAME_LEN: u32 = 4 * 1024 * 1024;
/// SEND 帧默认 ttl（中继跳数预算，防环）。
pub const DEFAULT_TTL: u8 = 16;
/// 握手整体超时（Hello/挑战/签名/ECDH 全流程）。
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// 协议版本（P2a=2：ECDH 加密链路 + version 字段）。
///
/// **无明文回落**：Hello 的 version 不一致即拒连——同版本才可互联（升级窗口
/// 内新旧节点互不可见，避免半加密连接）。
pub const PROTOCOL_VERSION: u32 = 2;

// ============================================================================
// 帧模型
// ============================================================================

/// 帧类型（线上为 snake_case 字符串）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameKind {
    /// 握手第 1 步：自我介绍 + ECDH 临时公钥（id 经签名验证）。
    Hello,
    /// 握手第 2 步：nonce 挑战。
    AuthChallenge,
    /// 握手第 3 步：对挑战的 ECDSA 签名（65 字节 r||s||v hex，覆盖临时公钥）。
    AuthResponse,
    /// 存活探测（req_id 关联 PONG）。
    Ping,
    /// 存活应答。
    Pong,
    /// Kademlia 查询：返回离 target 最近的 k 个已知节点（+观测端点八卦）。
    FindNode,
    /// FINDNODE 应答：节点描述列表 + 观测端点地址簿样本。
    Nodes,
    /// 应用消息（多跳中继，dst = 最终接收者）。
    Send,
    /// NAT 节点向公网对端注册："经你中继可达我"。
    RelayAnnounce,
    /// TCP 打洞第 1 消息（发起方 A →（共同中介转发）→ 目标 B）：
    /// 携带随机会话 token + A 的观测端点。B 若接受即回 [`FrameKind::Punch2`]。
    Punch1,
    /// TCP 打洞第 2 消息（B →（中介转发）→ A）：回显 token + B 的观测端点——
    /// 双方据此在约定时刻同时向对方观测端点发起 TCP 同时打开。
    Punch2,
    /// 节点元数据交互：注册表摘要广播（meta 组件每 6 tick 发给所有已连节点，
    /// dst = 对端；条目见 meta::MetaDigestEntry——收到即合并入库）。
    MetaGossip,
}

/// 长度前缀 JSON 信封——所有节点间消息的统一容器。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Frame {
    /// 帧类型。
    pub kind: FrameKind,
    /// 协议版本（缺省 0 = 旧版本/伪造——握手即时拒连）。
    #[serde(default)]
    pub version: u32,
    /// 发送方 NodeID。
    pub src: NodeId,
    /// 目标 NodeID（send/punch 帧 = 最终接收者；握手帧可为 null）。
    pub dst: Option<NodeId>,
    /// 剩余跳数预算（中继转发 -1；0 丢弃）。
    pub ttl: u8,
    /// 已穿越的中继跳数。
    pub hops: u8,
    /// kind 约定的 JSON 载荷。
    pub payload: serde_json::Value,
}

impl Frame {
    fn envelope(
        kind: FrameKind,
        src: &NodeId,
        dst: Option<NodeId>,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            kind,
            version: PROTOCOL_VERSION,
            src: src.clone(),
            dst,
            ttl: DEFAULT_TTL,
            hops: 0,
            payload,
        }
    }

    /// 应用消息帧（ttl=16 / hops=0 起点）。
    pub fn send(src: &NodeId, dst: &NodeId, payload: serde_json::Value) -> Self {
        Self::envelope(
            FrameKind::Send,
            src,
            Some(dst.clone()),
            serde_json::json!({ "payload": payload }),
        )
    }

    /// Hello（underlay：公网节点通告可拨地址；NAT 节点 null——不可直拨；
    /// eph：本端 ECDH 临时公钥 hex，防 MITM 签名转录本的组成部分）。
    pub fn hello(src: &NodeId, underlay: Option<String>, public: bool, eph_hex: &str) -> Self {
        Self::envelope(
            FrameKind::Hello,
            src,
            None,
            serde_json::json!({ "underlay": underlay, "public": public, "eph": eph_hex }),
        )
    }

    /// nonce 挑战。
    pub fn auth_challenge(src: &NodeId, req_id: u64, nonce: &str) -> Self {
        Self::envelope(
            FrameKind::AuthChallenge,
            src,
            None,
            serde_json::json!({ "req_id": req_id, "nonce": nonce }),
        )
    }

    /// 挑战签名应答（sig = 65 字节 r||s||v hex；签名覆盖 nonce+双方临时公钥）。
    pub fn auth_response(src: &NodeId, req_id: u64, sig65_hex: &str) -> Self {
        Self::envelope(
            FrameKind::AuthResponse,
            src,
            None,
            serde_json::json!({ "req_id": req_id, "signature": sig65_hex }),
        )
    }

    /// 存活探测。
    pub fn ping(src: &NodeId, dst: &NodeId, req_id: u64) -> Self {
        Self::envelope(
            FrameKind::Ping,
            src,
            Some(dst.clone()),
            serde_json::json!({ "req_id": req_id }),
        )
    }

    /// 存活应答（回显 req_id）。
    pub fn pong(src: &NodeId, dst: &NodeId, req_id: u64) -> Self {
        Self::envelope(
            FrameKind::Pong,
            src,
            Some(dst.clone()),
            serde_json::json!({ "req_id": req_id }),
        )
    }

    /// FINDNODE(target)。
    pub fn find_node(
        src: &NodeId,
        dst: &NodeId,
        req_id: u64,
        target: &crate::identity::OverlayAddr,
    ) -> Self {
        Self::envelope(
            FrameKind::FindNode,
            src,
            Some(dst.clone()),
            serde_json::json!({ "req_id": req_id, "target": target.to_hex() }),
        )
    }

    /// NODES 应答（节点描述列表 + 观测端点八卦样本，serde 见 kad::NodeInfo /
    /// endpoints::EndpointGossip）。
    pub fn nodes(
        src: &NodeId,
        dst: &NodeId,
        req_id: u64,
        nodes: &[crate::kad::NodeInfo],
        endpoints: &[EndpointGossip],
    ) -> Self {
        Self::envelope(
            FrameKind::Nodes,
            src,
            Some(dst.clone()),
            serde_json::json!({ "req_id": req_id, "nodes": nodes, "endpoints": endpoints }),
        )
    }

    /// 中继注册（src = 申请被中继的 NAT 节点，发给它的 relay）。
    pub fn relay_announce(src: &NodeId, dst: &NodeId) -> Self {
        Self::envelope(
            FrameKind::RelayAnnounce,
            src,
            Some(dst.clone()),
            serde_json::json!({}),
        )
    }

    /// 打洞第 1 消息：发起方 →（中介转交）→ 目标。`endpoints` = 发起方的
    /// 观测端点（地址交换所学到的"网络看我是什么 ip:port"）。
    pub fn punch1(src: &NodeId, dst: &NodeId, token: &str, endpoints: &[SocketAddr]) -> Self {
        Self::envelope(
            FrameKind::Punch1,
            src,
            Some(dst.clone()),
            serde_json::json!({ "token": token, "endpoints": endpoints }),
        )
    }

    /// 打洞第 2 消息：目标应答（回显 token + 自己的观测端点）。
    pub fn punch2(src: &NodeId, dst: &NodeId, token: &str, endpoints: &[SocketAddr]) -> Self {
        Self::envelope(
            FrameKind::Punch2,
            src,
            Some(dst.clone()),
            serde_json::json!({ "token": token, "endpoints": endpoints }),
        )
    }

    /// 元数据交互：注册表摘要（每连接一帧直发——不走路由/中继；收方在帧分发
    /// 处进 meta 模块合并，见 api::handle_frame 的 MetaGossip 分支）。
    pub fn meta_gossip(
        src: &NodeId,
        dst: &NodeId,
        entries: &[crate::meta::MetaDigestEntry],
    ) -> Self {
        Self::envelope(
            FrameKind::MetaGossip,
            src,
            Some(dst.clone()),
            serde_json::json!({ "entries": entries }),
        )
    }

    /// 取 payload 字段（缺失/类型不符 → None）。
    pub fn field(&self, key: &str) -> Option<&serde_json::Value> {
        self.payload.get(key)
    }

    pub fn req_id(&self) -> Option<u64> {
        self.field("req_id").and_then(|v| v.as_u64())
    }

    /// 应用载荷（仅 send 帧有义）。
    pub fn app_payload(&self) -> Option<&serde_json::Value> {
        self.field("payload")
    }

    /// 打洞载荷：(token, 观测端点)（仅 punch1/punch2 有义）。
    pub fn punch_payload(&self) -> Option<(String, Vec<SocketAddr>)> {
        let token = self.field("token")?.as_str()?.to_string();
        let endpoints =
            serde_json::from_value::<Vec<SocketAddr>>(self.field("endpoints")?.clone()).ok()?;
        Some((token, endpoints))
    }

    /// 元数据交互载荷：注册表摘要条目（仅 meta_gossip 帧有义；缺失/非法 → None）。
    pub fn meta_digest(&self) -> Option<Vec<crate::meta::MetaDigestEntry>> {
        serde_json::from_value::<Vec<crate::meta::MetaDigestEntry>>(self.field("entries")?.clone())
            .ok()
    }

    /// 中继转发变换：ttl-1 / hops+1。ttl 已尽 → None（调用方丢弃并告警）。
    #[must_use]
    pub fn hopped(&self) -> Option<Frame> {
        if self.ttl == 0 {
            return None;
        }
        let mut f = self.clone();
        f.ttl -= 1;
        f.hops += 1;
        Some(f)
    }

    /// JSON 编码（不含长度前缀）。
    pub fn encode_json(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    /// 从 JSON 字节解码。
    pub fn decode_json(bytes: &[u8]) -> Result<Self> {
        Ok(serde_json::from_slice(bytes)?)
    }
}

// ============================================================================
// 编解码（长度前缀读写；明文仅限握手帧，数据帧走加密通道）
// ============================================================================

/// 写一帧明文：4 字节大端长度 + JSON 信封（**仅握手阶段使用**）。
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &Frame) -> Result<()> {
    let body = frame.encode_json()?;
    let len = u32::try_from(body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame exceeds u32 length"))?;
    if len > MAX_FRAME_LEN {
        return Err(P2pError::FrameTooLarge(body.len()));
    }
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// 读一帧明文：4 字节大端长度 + JSON 信封（**仅握手阶段使用**）。
/// 超上限 / EOF / 损坏 → Err（调用方断连）。
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Frame> {
    let body = read_body(r).await?;
    Frame::decode_json(&body)
}

/// 写一帧密文：4 字节大端长度 + `nonce‖密文+标签`（握手后所有数据帧）。
pub async fn write_frame_enc<W: AsyncWrite + Unpin>(
    w: &mut W,
    cipher: &SessionCipher,
    frame: &Frame,
) -> Result<()> {
    let body = cipher.seal(&frame.encode_json()?)?;
    let len = u32::try_from(body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame exceeds u32 length"))?;
    if len > MAX_FRAME_LEN {
        return Err(P2pError::FrameTooLarge(body.len()));
    }
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await?;
    Ok(())
}

/// 读一帧密文并解密验签（GCM 标签不符 = 链路被篡改或密钥不符 → Err 断连）。
pub async fn read_frame_enc<R: AsyncRead + Unpin>(
    r: &mut R,
    cipher: &SessionCipher,
) -> Result<Frame> {
    let body = read_body(r).await?;
    let plain = cipher.open(&body)?;
    Frame::decode_json(&plain)
}

/// 读长度前缀帧体（明文/密文共用）。
async fn read_body<R: AsyncRead + Unpin>(r: &mut R) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_LEN {
        return Err(P2pError::FrameTooLarge(len as usize));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    Ok(body)
}

// ============================================================================
// 握手（双向 nonce 挑战-签名 + ECDH 密钥协商）
// ============================================================================

/// 握手时本端的自我描述。
pub struct HelloCtx<'a> {
    /// 本端身份（私钥——用于应答对端挑战）。
    pub identity: &'a NodeIdentity,
    /// 通告的可拨 underlay 地址（None = NAT/不可直拨，只能经中继）。
    pub advertise: Option<String>,
    /// 是否公网服务节点（承担 bootstrap/relay 职责）。
    pub public: bool,
}

/// 握手产物：对端身份与路由信息（已通过签名验证）。
#[derive(Debug, Clone)]
pub struct PeerHello {
    /// 对端 NodeID（签名已验证——确为其私钥持有人）。
    pub node_id: NodeId,
    /// 对端通告的 underlay（None = NAT 不可直拨）。
    pub underlay: Option<String>,
    /// 对端是否公网服务节点。
    pub public: bool,
}

/// 随机挑战 nonce（256-bit hex；CSPRNG，与 chain_auth 的 nonce 同生成方式）。
fn fresh_nonce() -> String {
    let mut buf = [0u8; 32];
    OsRngExt::fill_bytes(&mut buf);
    hex::encode(buf)
}

/// OsRng 别名垫片（避免函数体内 use 展开）。
struct OsRngExt;

impl OsRngExt {
    fn fill_bytes(buf: &mut [u8]) {
        use k256::elliptic_curve::rand_core::{OsRng, RngCore};
        OsRng.fill_bytes(buf);
    }
}

/// 对称执行握手：hello → 双向挑战 → 双向验签（签名覆盖双方临时公钥）→
/// ECDH 派生会话密钥。成功 = 对端确持有其 NodeID 私钥 **且** 拿到与对端一致的
/// 加密通道（返回 [`SessionCipher`]，之后所有帧必须走
/// [`write_frame_enc`]/[`read_frame_enc`]）。
///
/// `read`/`write` 通常是同一 TcpStream 的两半；拆参便于测试注入 duplex 内存流。
pub async fn handshake<R, W>(
    read: &mut R,
    write: &mut W,
    ctx: &HelloCtx<'_>,
) -> std::result::Result<(PeerHello, SessionCipher), P2pError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let self_id = ctx.identity.node_id();
    let eph = EphemeralKey::generate();
    let fut = async {
        // 1) 互发 Hello（双方先发再读，无序等待）——版本门在第一时间拦截
        write_frame(
            write,
            &Frame::hello(
                &self_id,
                ctx.advertise.clone(),
                ctx.public,
                eph.public_hex(),
            ),
        )
        .await?;
        let peer_hello = read_frame(read).await?;
        if peer_hello.kind != FrameKind::Hello {
            return Err(P2pError::Handshake {
                peer: "unknown".into(),
                reason: format!("expected hello, got {:?}", peer_hello.kind),
            });
        }
        if peer_hello.version != PROTOCOL_VERSION {
            // 无明文回落：异版本即拒连
            return Err(P2pError::VersionMismatch {
                ours: PROTOCOL_VERSION,
                theirs: peer_hello.version,
            });
        }
        let peer_id = peer_hello.src.clone();
        let underlay = peer_hello
            .field("underlay")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let public = peer_hello
            .field("public")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let peer_eph = peer_hello
            .field("eph")
            .and_then(|v| v.as_str())
            .ok_or_else(|| P2pError::Handshake {
                peer: peer_id.to_hex(),
                reason: "hello missing ephemeral key (pre-P2a node?)".into(),
            })?
            .to_string();

        // 2) 我方挑战
        let my_nonce = fresh_nonce();
        write_frame(write, &Frame::auth_challenge(&self_id, 1, &my_nonce)).await?;
        // 3) 收对端挑战并签名应答（签名覆盖 nonce + 双方临时公钥 → 防 MITM）
        let their_challenge = read_frame(read).await?;
        if their_challenge.kind != FrameKind::AuthChallenge || their_challenge.src != peer_id {
            return Err(P2pError::Handshake {
                peer: peer_id.to_hex(),
                reason: "expected auth_challenge from hello peer".into(),
            });
        }
        let their_nonce = their_challenge
            .field("nonce")
            .and_then(|v| v.as_str())
            .ok_or_else(|| P2pError::Handshake {
                peer: peer_id.to_hex(),
                reason: "challenge missing nonce".into(),
            })?
            .to_string();
        let req_id = their_challenge.req_id().unwrap_or(0);
        let transcript = ecdh_transcript(&their_nonce, eph.public_hex(), &peer_eph);
        write_frame(
            write,
            &Frame::auth_response(
                &self_id,
                req_id,
                &hex::encode(ctx.identity.sign(&transcript)),
            ),
        )
        .await?;
        // 4) 验证对端对我方挑战的签名（chain_auth 验签契约；转录本含双方 eph）
        let their_response = read_frame(read).await?;
        if their_response.kind != FrameKind::AuthResponse || their_response.src != peer_id {
            return Err(P2pError::Handshake {
                peer: peer_id.to_hex(),
                reason: "expected auth_response from hello peer".into(),
            });
        }
        let sig_hex = their_response
            .field("signature")
            .and_then(|v| v.as_str())
            .ok_or_else(|| P2pError::Handshake {
                peer: peer_id.to_hex(),
                reason: "response missing signature".into(),
            })?;
        let sig = hex::decode(sig_hex).map_err(|_| P2pError::Handshake {
            peer: peer_id.to_hex(),
            reason: "signature not hex".into(),
        })?;
        let expected_transcript = ecdh_transcript(&my_nonce, &peer_eph, eph.public_hex());
        if !peer_id.verify_signature(&expected_transcript, &sig) {
            return Err(P2pError::Handshake {
                peer: peer_id.to_hex(),
                reason: "challenge signature invalid (peer does not hold the NodeID key, \
                         or ephemeral key was tampered in transit)"
                    .into(),
            });
        }
        // 5) ECDH 派生会话密钥（nonce canonical 排序——两侧与拨号方向无关）
        let (lo, hi) = if my_nonce <= their_nonce {
            (&my_nonce, &their_nonce)
        } else {
            (&their_nonce, &my_nonce)
        };
        let cipher = eph.derive_session(&peer_eph, lo, hi)?;
        Ok((
            PeerHello {
                node_id: peer_id,
                underlay,
                public,
            },
            cipher,
        ))
    };
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, fut).await {
        Ok(res) => res,
        Err(_) => Err(P2pError::Handshake {
            peer: "unknown".into(),
            reason: format!("timeout after {HANDSHAKE_TIMEOUT:?}"),
        }),
    }
}

// ============================================================================
// 单元测——帧编解码 / ttl·hops 语义 / ECDH 握手双向认证与防篡改 / 加密帧
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::NodeIdentity;
    use tokio::io::duplex;

    fn id(seed: u8) -> (NodeIdentity, NodeId) {
        let n = NodeIdentity::from_seed(&[seed; 32]);
        (n.clone(), n.node_id())
    }

    /// 真实回环 TCP 对：两侧各拆 (read, write)——对称握手的 halves 语义与生产
    /// 代码一致（tokio 1.53 的 DuplexStream 无 try_clone，用真 socket 最保真）。
    async fn tcp_pair() -> (
        (
            tokio::net::tcp::OwnedReadHalf,
            tokio::net::tcp::OwnedWriteHalf,
        ),
        (
            tokio::net::tcp::OwnedReadHalf,
            tokio::net::tcp::OwnedWriteHalf,
        ),
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let (cr, cw) = client.into_split();
        let (sr, sw) = server.into_split();
        ((cr, cw), (sr, sw))
    }

    /// 两对 TCP 组成 A↔M↔B 链路，M 为字节级中继：可对途经帧做篡改（MITM 测试）。
    async fn tamper_pair(
        mutate_hello_eph: bool,
    ) -> (
        (
            tokio::net::tcp::OwnedReadHalf,
            tokio::net::tcp::OwnedWriteHalf,
        ),
        (
            tokio::net::tcp::OwnedReadHalf,
            tokio::net::tcp::OwnedWriteHalf,
        ),
    ) {
        let ((a_r, a_w), (m1_r, m1_w)) = tcp_pair().await;
        let ((m2_r, m2_w), (b_r, b_w)) = tcp_pair().await;
        tokio::spawn(async move {
            let mut m1_r = m1_r;
            let mut m1_w = m1_w;
            let mut m2_r = m2_r;
            let mut m2_w = m2_w;
            // A → B 方向（篡改该方向 hello 的 eph）
            let f1 = async {
                while let Ok(mut f) = read_frame(&mut m1_r).await {
                    if mutate_hello_eph && f.kind == FrameKind::Hello {
                        f.payload["eph"] = serde_json::json!(EphemeralKey::generate().public_hex());
                    }
                    if write_frame(&mut m2_w, &f).await.is_err() {
                        break;
                    }
                }
            };
            // B → A 方向（不篡改）
            let f2 = async {
                while let Ok(f) = read_frame(&mut m2_r).await {
                    if write_frame(&mut m1_w, &f).await.is_err() {
                        break;
                    }
                }
            };
            tokio::join!(f1, f2);
        });
        ((a_r, a_w), (b_r, b_w))
    }

    // 1. 帧往返：write → read 字节级还原（含长度前缀）；字段访问器正确；
    //    version 字段恒为当前协议版本
    #[tokio::test]
    async fn frame_roundtrip_over_duplex() {
        let (_, alice) = id(1);
        let (_, bob) = id(2);
        let frame = Frame::send(
            &alice,
            &bob,
            serde_json::json!({"text": "你好 NexOS", "n": 42}),
        );
        assert_eq!(frame.version, PROTOCOL_VERSION);
        let (mut a, mut b) = duplex(64 << 10);
        write_frame(&mut a, &frame).await.unwrap();
        let back = read_frame(&mut b).await.unwrap();
        assert_eq!(back, frame);
        assert_eq!(back.kind, FrameKind::Send);
        assert_eq!(back.src, alice);
        assert_eq!(back.dst.as_ref(), Some(&bob));
        assert_eq!(back.ttl, DEFAULT_TTL);
        assert_eq!(back.hops, 0);
        assert_eq!(back.app_payload().unwrap()["n"], 42);
    }

    // 2. 控制帧构造器：payload schema 与 req_id 提取（含 punch / nodes 扩展）
    #[tokio::test]
    async fn control_frame_constructors_payload_schema() {
        let (_, a) = id(3);
        let (_, b) = id(4);
        let target = crate::identity::OverlayAddr::random();
        let f = Frame::find_node(&a, &b, 77, &target);
        assert_eq!(f.req_id(), Some(77));
        assert_eq!(
            f.field("target").unwrap().as_str(),
            Some(target.to_hex().as_str())
        );
        let ping = Frame::ping(&a, &b, 9);
        let pong = Frame::pong(&b, &a, ping.req_id().unwrap());
        assert_eq!(pong.req_id(), ping.req_id());
        let eph = EphemeralKey::generate();
        let hello = Frame::hello(&a, Some("203.0.113.7:7070".into()), true, eph.public_hex());
        assert_eq!(
            hello.field("underlay").unwrap().as_str(),
            Some("203.0.113.7:7070")
        );
        assert_eq!(hello.field("public").unwrap().as_bool(), Some(true));
        assert_eq!(hello.field("eph").unwrap().as_str(), Some(eph.public_hex()));
        // nodes：节点列表 + 端点八卦
        let eps = vec![
            EndpointGossip::new(b.clone(), "198.51.100.9:7070".parse().unwrap()),
            EndpointGossip::new(a.clone(), "198.51.100.10:7071".parse().unwrap()),
        ];
        let nodes = Frame::nodes(&a, &b, 5, &[], &eps);
        let parsed = serde_json::from_value::<Vec<EndpointGossip>>(
            nodes.field("endpoints").unwrap().clone(),
        )
        .unwrap();
        assert_eq!(parsed, eps);
        // punch 载荷往返
        let addrs: Vec<SocketAddr> = vec![
            "203.0.113.1:40000".parse().unwrap(),
            "203.0.113.2:40001".parse().unwrap(),
        ];
        let p1 = Frame::punch1(&a, &b, "deadbeef", &addrs);
        let (token, got) = p1.punch_payload().unwrap();
        assert_eq!(token, "deadbeef");
        assert_eq!(got, addrs);
        let p2 = Frame::punch2(&b, &a, "cafebabe", &addrs[..1]);
        assert_eq!(p2.kind, FrameKind::Punch2);
        // meta_gossip：注册表摘要载荷往返（元数据交互——收方合并入库）。
        // 第三条为自广播式无地址条目（对端未配置 advertise）：序列化省略 addr
        // 字段，反序列化回 None；旧字段格式（裸地址串）回 Some——线格式双态兼容
        let digest = vec![
            crate::meta::MetaDigestEntry {
                id: b.clone(),
                addr: Some(addrs[0]),
                last_seen: 42,
                alive: true,
                verified: true,
                exit_offered: false,
            },
            crate::meta::MetaDigestEntry {
                id: a.clone(),
                addr: Some(addrs[1]),
                last_seen: 7,
                alive: false,
                verified: false,
                exit_offered: false,
            },
            crate::meta::MetaDigestEntry {
                id: b.clone(),
                addr: None,
                last_seen: 9,
                alive: true,
                verified: true,
                exit_offered: false,
            },
        ];
        let mg = Frame::meta_gossip(&a, &b, &digest);
        assert_eq!(mg.kind, FrameKind::MetaGossip);
        assert_eq!(mg.meta_digest().unwrap(), digest);
        assert!(
            Frame::ping(&a, &b, 1).meta_digest().is_none(),
            "非 meta 帧无此载荷"
        );
        // JSON 往返保持 kind
        for f in [
            f,
            ping,
            pong,
            hello,
            nodes,
            p1,
            p2,
            mg,
            Frame::relay_announce(&a, &b),
        ] {
            let dec = Frame::decode_json(&f.encode_json().unwrap()).unwrap();
            assert_eq!(dec.kind, f.kind);
            assert_eq!(dec.version, PROTOCOL_VERSION);
        }
    }

    // 3. hopped()：ttl 递减 / hops 递增；ttl=0 → None（防环丢弃）
    #[test]
    fn hopped_ttl_decrement_and_drop() {
        let (_, a) = id(5);
        let (_, b) = id(6);
        let mut f = Frame::send(&a, &b, serde_json::json!({"x": 1}));
        for i in 1..=3u8 {
            f = f.hopped().expect("ttl 未尽应可转发");
            assert_eq!(f.ttl, DEFAULT_TTL - i);
            assert_eq!(f.hops, i);
        }
        let mut dead = Frame::send(&a, &b, serde_json::json!({}));
        dead.ttl = 0;
        assert!(dead.hopped().is_none(), "ttl=0 必须丢弃");
        let mut one = Frame::send(&a, &b, serde_json::json!({}));
        one.ttl = 1;
        assert_eq!(one.hopped().unwrap().ttl, 0);
    }

    // 4. 握手双向认证 + ECDH：双方拿到已验证对端身份，且派生**同一**会话密钥
    #[tokio::test]
    async fn handshake_mutual_auth_succeeds() {
        let (alice_id, alice_node) = id(0xA1);
        let (bob_id, bob_node) = id(0xB2);
        let (a, b) = tcp_pair().await;
        let (mut a_r, mut a_w) = a;
        let (mut b_r, mut b_w) = b;
        let h1 = tokio::spawn(async move {
            handshake(
                &mut a_r,
                &mut a_w,
                &HelloCtx {
                    identity: &alice_id,
                    advertise: Some("203.0.113.1:7070".into()),
                    public: true,
                },
            )
            .await
        });
        let h2 = tokio::spawn(async move {
            handshake(
                &mut b_r,
                &mut b_w,
                &HelloCtx {
                    identity: &bob_id,
                    advertise: None,
                    public: false,
                },
            )
            .await
        });
        let (pa, ca) = h1.await.unwrap().unwrap();
        let (pb, cb) = h2.await.unwrap().unwrap();
        assert_eq!(pa.node_id, bob_node, "Alice 验证到的对端 = Bob");
        assert_eq!(pb.node_id, alice_node);
        // pa 是 Alice 视角的 Bob（NAT：无 underlay、非公网）；pb 是 Bob 视角的 Alice
        assert!(!pa.public && pa.underlay.is_none());
        assert!(pb.public && pb.underlay.as_deref() == Some("203.0.113.1:7070"));
        // ECDH 对账：两侧独立派生 → 同一会话密钥（且非全零）
        assert_eq!(ca, cb, "握手双方必须派生同一会话密钥");
        assert_ne!(ca, SessionCipher::from_key(&[0u8; 32]));
    }

    // 5. 握手拒绝：冒充他人 NodeID（签名验证失败即断）
    #[tokio::test]
    async fn handshake_rejects_impersonated_identity() {
        let (honest, honest_node) = id(0xC3);
        // Mallory 声称自己是 honest_node，但只有自己的私钥
        let (mallory, _) = id(0xD4);
        let (a, b) = tcp_pair().await;
        let (mut a_r, mut a_w) = a;
        let (mut b_r, mut b_w) = b;
        let claimed = honest_node.clone();
        let fake_eph = EphemeralKey::generate();
        let fake_eph_hex = fake_eph.public_hex().to_string();
        let h1 = tokio::spawn(async move {
            handshake(
                &mut a_r,
                &mut a_w,
                &HelloCtx {
                    identity: &honest,
                    advertise: None,
                    public: false,
                },
            )
            .await
        });
        let h2 = tokio::spawn(async move {
            // 冒充方完整镜像协议脚本（否则 honest 在帧序检查就拒了，测不到签名验证）：
            // w hello → r hello → w challenge → r challenge → w forged response → r response
            write_frame(
                &mut b_w,
                &Frame::hello(&claimed, None, false, &fake_eph_hex),
            )
            .await
            .unwrap();
            let peer_hello = read_frame(&mut b_r).await.unwrap();
            assert_eq!(peer_hello.kind, FrameKind::Hello);
            let peer_eph = peer_hello
                .field("eph")
                .unwrap()
                .as_str()
                .unwrap()
                .to_string();
            write_frame(&mut b_w, &Frame::auth_challenge(&claimed, 1, "00"))
                .await
                .unwrap();
            let challenge = read_frame(&mut b_r).await.unwrap();
            assert_eq!(challenge.kind, FrameKind::AuthChallenge);
            let nonce = challenge
                .field("nonce")
                .unwrap()
                .as_str()
                .unwrap()
                .to_string();
            // Mallory 没有对端声称身份的转录本知识也无所谓——它只有错误私钥
            let transcript = ecdh_transcript(&nonce, &fake_eph_hex, &peer_eph);
            let forged = hex::encode(mallory.sign(&transcript)); // 错误私钥的签名
            write_frame(
                &mut b_w,
                &Frame::auth_response(&claimed, challenge.req_id().unwrap(), &forged),
            )
            .await
            .unwrap();
            let _ = read_frame(&mut b_r).await; // honest 的 response（结果无关紧要）
        });
        let res = h1.await.unwrap();
        assert!(
            matches!(&res, Err(P2pError::Handshake { reason, .. }) if reason.contains("invalid")),
            "冒充身份必须被拒，实测 {res:?}"
        );
        drop(h2);
    }

    // 6. ECDH 防 MITM（正反）：中途替换 Hello 里的临时公钥 → 验签失败。
    //    任一方向的 eph 被换，签名转录本两侧不再一致——诚实双方握手都失败。
    #[tokio::test]
    async fn ecdh_handshake_rejects_tampered_ephemeral_key() {
        // 方向 A→B 的 hello eph 被篡改：B 签的转录本用的是篡改后的 eph_A，
        // A 验签时用自己原始 eph_A → 不一致 → 双方都失败
        let (alice_id, _) = id(0xE1);
        let (bob_id, _) = id(0xE2);
        let (a, b) = tamper_pair(true).await;
        let (mut a_r, mut a_w) = a;
        let (mut b_r, mut b_w) = b;
        let ha = tokio::spawn(async move {
            handshake(
                &mut a_r,
                &mut a_w,
                &HelloCtx {
                    identity: &alice_id,
                    advertise: None,
                    public: false,
                },
            )
            .await
        });
        let hb = tokio::spawn(async move {
            handshake(
                &mut b_r,
                &mut b_w,
                &HelloCtx {
                    identity: &bob_id,
                    advertise: None,
                    public: false,
                },
            )
            .await
        });
        let ra = ha.await.unwrap();
        let rb = hb.await.unwrap();
        for (side, res) in [("A", &ra), ("B", &rb)] {
            assert!(
                matches!(res, Err(P2pError::Handshake { ref reason, .. })
                    if reason.contains("invalid") || reason.contains("ephemeral")),
                "侧 {side} 必须检出临时公钥被篡改，实测 {res:?}"
            );
        }
    }

    // 7. 版本门：异版本 Hello（旧版本节点）→ 拒连（无明文回落）
    #[tokio::test]
    async fn handshake_rejects_version_mismatch() {
        let (honest, _) = id(0xF1);
        let (_, old_node) = id(0xF2);
        let (a, b) = tcp_pair().await;
        let (mut a_r, mut a_w) = a;
        let (mut b_r, mut b_w) = b;
        let h1 = tokio::spawn(async move {
            handshake(
                &mut a_r,
                &mut a_w,
                &HelloCtx {
                    identity: &honest,
                    advertise: None,
                    public: false,
                },
            )
            .await
        });
        let old = old_node.clone();
        let _ = tokio::spawn(async move {
            // 旧版本节点（version=1）镜像握手脚本
            let eph = EphemeralKey::generate();
            let mut hello = Frame::hello(&old, None, false, eph.public_hex());
            hello.version = 1; // 旧版本
            write_frame(&mut b_w, &hello).await.unwrap();
            let _ = read_frame(&mut b_r).await; // honest 的 hello
            write_frame(&mut b_w, &Frame::auth_challenge(&old, 1, "00"))
                .await
                .unwrap();
            let _ = read_frame(&mut b_r).await;
            // honest 侧已拒——后续写入失败无所谓
            let _ = b_w.shutdown().await;
        })
        .await;
        let res = h1.await.unwrap();
        assert!(
            matches!(&res, Err(P2pError::VersionMismatch { ours, theirs }) if *ours == PROTOCOL_VERSION && *theirs == 1),
            "异版本必须拒连，实测 {res:?}"
        );
    }

    // 8. 加密帧往返：握手后两侧互发互收还原；错密钥解密失败
    #[tokio::test]
    async fn encrypted_frame_roundtrip_after_handshake() {
        let (alice_id, alice_node) = id(0x11);
        let (bob_id, bob_node) = id(0x22);
        let (a, b) = tcp_pair().await;
        let (mut a_r, mut a_w) = a;
        let (mut b_r, mut b_w) = b;
        let ctx_a = HelloCtx {
            identity: &alice_id,
            advertise: None,
            public: false,
        };
        let ctx_b = HelloCtx {
            identity: &bob_id,
            advertise: None,
            public: false,
        };
        let (ha, hb) = tokio::join!(
            handshake(&mut a_r, &mut a_w, &ctx_a),
            handshake(&mut b_r, &mut b_w, &ctx_b),
        );
        let ((pa, ca), (pb, cb)) = (ha.unwrap(), hb.unwrap());
        assert_eq!(pa.node_id, bob_node);
        assert_eq!(pb.node_id, alice_node);
        assert_eq!(ca, cb);
        // 双向密文往返
        let f1 = Frame::send(&alice_node, &bob_node, serde_json::json!({"n": 1}));
        write_frame_enc(&mut a_w, &ca, &f1).await.unwrap();
        assert_eq!(read_frame_enc(&mut b_r, &cb).await.unwrap(), f1);
        let f2 = Frame::ping(&bob_node, &alice_node, 9);
        write_frame_enc(&mut b_w, &cb, &f2).await.unwrap();
        assert_eq!(read_frame_enc(&mut a_r, &ca).await.unwrap(), f2);
        // 错误密钥（另一会话）解不开本会话密文
        let other = SessionCipher::from_key(&[9u8; 32]);
        write_frame_enc(&mut a_w, &ca, &f1).await.unwrap();
        assert!(
            read_frame_enc(&mut b_r, &other).await.is_err(),
            "错误会话密钥必须解密失败"
        );
    }

    // 9. 超长帧防护：> MAX_FRAME_LEN 拒读（明文/密文同限）
    #[tokio::test]
    async fn oversize_frame_rejected() {
        let cipher = SessionCipher::from_key(&[1u8; 32]);
        let (mut a, mut b) = duplex(1024);
        a.write_all(&(MAX_FRAME_LEN + 1).to_be_bytes())
            .await
            .unwrap();
        let err = read_frame(&mut b).await.unwrap_err();
        assert!(
            matches!(err, P2pError::FrameTooLarge(n) if n == (MAX_FRAME_LEN + 1) as usize),
            "实测 {err:?}"
        );
        // 密文读路径同限
        let (mut c, mut d) = duplex(1024);
        c.write_all(&(MAX_FRAME_LEN + 1).to_be_bytes())
            .await
            .unwrap();
        let err = read_frame_enc(&mut d, &cipher).await.unwrap_err();
        assert!(matches!(err, P2pError::FrameTooLarge(_)));
    }
}
