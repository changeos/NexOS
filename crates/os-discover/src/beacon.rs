//! Beacon 防伪签名——challenge/nonce 生成与比对 + 真实 ed25519 验签
//!
//! 决策依据：规划文档 §3.14 / 规格书 §3 —— 节点在 LAN 广播 beacon 时用自身私钥
//! 签名 beacon 载荷，发现方用预置/凭证公钥校验，避免伪造节点混入。
//!
//! 本模块把"beacon 载荷规范化 + challenge/nonce 生成与比对 + ed25519 真实验签"
//! 沉淀为纯逻辑，便于单测。
//!
//! ## 依赖接入状态（ADR-DEPS-001）
//! - **ed25519 验签**：已接通——[`verify_beacon_signature`] 在传入公钥时用
//!   `ed25519-dalek` 的 `verify_strict` 做真实密码学校验。
//! - **nonce / 密钥对生成**：用 `rand_core::OsRng`（CSPRNG）。
//! - **公钥指纹**：`sha2::Sha256`（与 mTLS 证书指纹算法一致，便于上层比对外部信任源）。
//! - **mDNS 组播 / mTLS 握手**：依赖 mdns-sd / rustls（未在 workspace 注册，P2），
//!   仍留 `TODO`——本模块不引入。
//!
//! ## 公钥注入约定
//! [`verify_beacon_signature`] 接受 `Option<&VerifyingKey>`：
//! - `Some(pk)`：真实 ed25519 `verify_strict`（生产路径——MdnsDiscovery 拿到对端
//!   预置公钥/凭证公钥后传入）。
//! - `None`：仅做载荷与签名的**结构校验**（mdns-sd 未接通前的骨架回退，保留原 40 测
//!   行为不变；真实组播路径接通后不应再传 None）。

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use os_core::{DateTime, NodeId};
use rand_core::{CryptoRngCore, OsRng};
use sha2::{Digest, Sha256};

use crate::discovery::PeerNode;

// ----------------------------------------------------------------------------
// hex 编解码（内联，避免引入未注册的 hex crate）
// ----------------------------------------------------------------------------

/// hex 编码小写（ed25519 签名 64B → 128 字符；公钥 32B → 64 字符）。
///
/// 不引入 `hex` crate（未在 workspace 注册，红线：不虚构依赖）。
pub fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// hex 解码（大小写不敏感）。返回 `None` 当含非 hex 字符或长度非偶数。
pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_val(bytes[i])?;
        let lo = hex_val(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ----------------------------------------------------------------------------
// Beacon 载荷
// ----------------------------------------------------------------------------

/// beacon 广播载荷（规范化后的待签名内容）
///
/// 规则：对 `node_id` / `endpoints` / `valid_until` / `nonce` 按固定顺序拼接，
/// 用 `\n` 分隔，作为待签名/比对的稳定字节序列。任一字段变动即改变签名输入，
/// 防止重放（nonce）与字段替换。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeaconPayload {
    /// 广播节点 ID
    pub node_id: NodeId,
    /// 接入端点（排序后拼接，避免顺序差异导致签名不一致）
    pub endpoints: Vec<String>,
    /// beacon 有效期截止（过期 beacon 应被丢弃）
    pub valid_until: DateTime,
    /// 一次性 nonce（防重放，每次广播递增/随机）
    pub nonce: u64,
}

impl BeaconPayload {
    /// 规范化为待签名字节序列（确定性：字段固定顺序、端点排序）
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut endpoints = self.endpoints.clone();
        endpoints.sort();
        let joined = endpoints.join(",");
        format!(
            "{}\n{}\n{}\n{}",
            self.node_id,
            joined,
            self.valid_until.timestamp_millis(),
            self.nonce
        )
        .into_bytes()
    }
}

// ----------------------------------------------------------------------------
// challenge / nonce 生成与比对
// ----------------------------------------------------------------------------

/// 生成一个新的 challenge nonce（CSPRNG 真随机，64 位）。
///
/// 用 `rand_core::OsRng`（操作系统级熵源）替代早期的时间戳占位——避免被攻击者
/// 预测/重放。发现方应记录最近见过的 nonce 并配合 [`is_nonce_fresh`] 拒绝重放。
pub fn generate_challenge_nonce() -> u64 {
    generate_challenge_nonce_with(&mut OsRng)
}

/// 用注入的 CSPRNG 生成 challenge nonce（便于确定性测试）。
pub fn generate_challenge_nonce_with<R: CryptoRngCore + ?Sized>(csprng: &mut R) -> u64 {
    csprng.next_u64()
}

/// 比对 challenge nonce 是否新鲜（在 `max_age_ms` 毫秒内）
///
/// 防重放：发现方记录最近见过的 nonce，过期或已用过的 nonce 视为陈旧。
pub fn is_nonce_fresh(nonce: u64, now: u64, max_age_ms: u64) -> bool {
    // 防御性：nonce 大于 now 视为时钟漂移/伪造未来时间，判为不新鲜
    nonce <= now && now.saturating_sub(nonce) <= max_age_ms
}

/// 校验 beacon 是否过期（`valid_until` 早于 `now`）
pub fn is_expired(valid_until: DateTime, now: DateTime) -> bool {
    valid_until < now
}

// ----------------------------------------------------------------------------
// ed25519 密钥/签名助手（签名侧）
// ----------------------------------------------------------------------------

/// 生成一对新的 ed25519 密钥（私钥 + 公钥），CSPRNG = OsRng。
///
/// 私钥由广播节点持有（用于签名 beacon 载荷）；公钥分发给发现方（用于验签）。
/// 真实部署中私钥落盘由 os-security 管理（本 agent 不下沉密钥存储）。
pub fn generate_keypair() -> (SigningKey, VerifyingKey) {
    generate_keypair_with(&mut OsRng)
}

/// 用注入的 CSPRNG 生成密钥对（便于确定性测试）。
pub fn generate_keypair_with<R: CryptoRngCore + ?Sized>(
    csprng: &mut R,
) -> (SigningKey, VerifyingKey) {
    let signing = SigningKey::generate(csprng);
    let verifying = signing.verifying_key();
    (signing, verifying)
}

/// 用私钥签名 beacon 载荷，返回签名的小写 hex（128 字符）。
///
/// 签名输入为 [`BeaconPayload::canonical_bytes`]——任一字段变动签名即变。
pub fn sign_beacon(signing_key: &SigningKey, payload: &BeaconPayload) -> String {
    let sig: Signature = signing_key.sign(&payload.canonical_bytes());
    hex_encode(&sig.to_bytes())
}

/// 计算公钥的 SHA-256 指纹（hex，64 字符）。
///
/// 与 mTLS 证书指纹（SHA-256）算法一致，便于上层把"beacon 公钥指纹"与
/// "mTLS 对端证书指纹"做关联校验（同一节点两路信任源应可对齐）。
pub fn pubkey_fingerprint(verifying_key: &VerifyingKey) -> String {
    let mut hasher = Sha256::new();
    hasher.update(verifying_key.to_bytes());
    hex_encode(&hasher.finalize())
}

// ----------------------------------------------------------------------------
// 签名校验（结构校验 + 真实 ed25519）
// ----------------------------------------------------------------------------

/// beacon 签名校验结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeaconVerifyOutcome {
    /// 校验通过
    Ok,
    /// 签名缺失（PeerNode.beacon_signature == None）
    Missing,
    /// 签名格式无效（无法解码为 hex / 长度不对）
    Malformed,
    /// beacon 已过期
    Expired,
    /// 节点 ID 与载荷不匹配（防字段替换）
    NodeIdMismatch,
    /// ed25519 验签失败（公钥不匹配 / 签名被篡改 / 载荷被篡改）
    BadSignature,
}

/// 校验单个 `PeerNode` 的 beacon 签名与新鲜度。
///
/// 参数：
/// - `peer`：发现的节点（含 `beacon_signature` hex 串）
/// - `payload`：与该 beacon 对应的规范化载荷
/// - `now`：当前时间（过期判定基准）
/// - `pubkey`：该节点预置/凭证公钥——
///   - `Some(pk)`：执行**真实 ed25519 `verify_strict`**（生产路径）。
///   - `None`：仅做载荷与签名的**结构校验**（mdns-sd 未接通前的骨架回退）。
///
/// 校验顺序（短路）：node_id 一致 → 未过期 → 签名存在 → 签名格式合法 →（若有公钥）密码学验签。
/// 任一失败返回对应 [`BeaconVerifyOutcome`]，全部通过返回 [`BeaconVerifyOutcome::Ok`]。
pub fn verify_beacon_signature(
    peer: &PeerNode,
    payload: &BeaconPayload,
    now: DateTime,
    pubkey: Option<&VerifyingKey>,
) -> BeaconVerifyOutcome {
    if peer.node_id != payload.node_id {
        return BeaconVerifyOutcome::NodeIdMismatch;
    }
    if is_expired(payload.valid_until, now) {
        return BeaconVerifyOutcome::Expired;
    }
    let sig_hex = match &peer.beacon_signature {
        None => return BeaconVerifyOutcome::Missing,
        Some(s) if s.trim().is_empty() => return BeaconVerifyOutcome::Malformed,
        Some(s) => s,
    };
    // ed25519 签名 = 64 字节 = 128 hex 字符
    let sig_bytes = match hex_decode(sig_hex) {
        Some(b) if b.len() == 64 => b,
        _ => return BeaconVerifyOutcome::Malformed,
    };
    // 结构校验通过。无公钥 → 骨架回退（mdns 未接通前保留旧行为）。
    let Some(pk) = pubkey else {
        return BeaconVerifyOutcome::Ok;
    };
    // 真实 ed25519 验签：verify_strict 拒绝 malleable 签名（更严格于 RFC 8032 的宽松验证）。
    let sig = match Signature::from_slice(&sig_bytes) {
        Ok(s) => s,
        Err(_) => return BeaconVerifyOutcome::Malformed,
    };
    match pk.verify_strict(&payload.canonical_bytes(), &sig) {
        Ok(()) => BeaconVerifyOutcome::Ok,
        Err(_) => BeaconVerifyOutcome::BadSignature,
    }
}

/// 生成一个伪签名（仅测试用：用 node_id 重复填充到 128 hex 字符）
///
/// 便于 mock/测试构造"结构合法"的 beacon_signature 而无需真实密钥对。
/// 真实 ed25519 路径下该伪签名会被 [`BeaconVerifyOutcome::BadSignature`] 拒绝。
#[cfg(any(test, feature = "mock"))]
pub fn pseudo_signature(node_id: &NodeId) -> String {
    let seed = node_id.as_str();
    let mut out = String::with_capacity(128);
    let mut i = 0;
    let hex = b"0123456789abcdef";
    while out.len() < 128 {
        let b = seed.as_bytes()[i % seed.len().max(1)];
        // 用 node_id 字节做确定性 hex 映射
        out.push(hex[((b as usize) + i) % 16] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::NodeCapabilities;
    use os_core::Utc;

    fn peer_with_sig(sig: Option<&str>) -> PeerNode {
        PeerNode {
            node_id: NodeId::new("node-A"),
            endpoints: vec!["10.0.0.1:8443".into()],
            version: "1.0.0".into(),
            arch: "x86_64".into(),
            capabilities: NodeCapabilities::minimal(),
            beacon_signature: sig.map(String::from),
        }
    }

    // —— 原有结构校验测试（保留 40 测，骨架回退路径 None）——

    #[test]
    fn canonical_bytes_deterministic_regardless_of_endpoint_order() {
        let now = Utc::now();
        let p1 = BeaconPayload {
            node_id: NodeId::new("n"),
            endpoints: vec!["a:1".into(), "b:2".into()],
            valid_until: now,
            nonce: 5,
        };
        let p2 = BeaconPayload {
            node_id: NodeId::new("n"),
            endpoints: vec!["b:2".into(), "a:1".into()],
            valid_until: now,
            nonce: 5,
        };
        assert_eq!(p1.canonical_bytes(), p2.canonical_bytes());
    }

    #[test]
    fn nonce_freshness_boundary() {
        assert!(is_nonce_fresh(100, 150, 100));
        assert!(is_nonce_fresh(100, 200, 100)); // 刚好 max_age
        assert!(!is_nonce_fresh(99, 200, 100)); // 超出 1ms
        assert!(!is_nonce_fresh(201, 200, 100)); // 未来时间
    }

    #[test]
    fn verify_missing_sig() {
        let peer = peer_with_sig(None);
        let payload = BeaconPayload {
            node_id: NodeId::new("node-A"),
            endpoints: vec![],
            valid_until: Utc::now() + chrono::Duration::seconds(60),
            nonce: 1,
        };
        assert_eq!(
            verify_beacon_signature(&peer, &payload, Utc::now(), None),
            BeaconVerifyOutcome::Missing
        );
    }

    #[test]
    fn verify_malformed_sig() {
        let peer = peer_with_sig(Some("nothex"));
        let payload = BeaconPayload {
            node_id: NodeId::new("node-A"),
            endpoints: vec![],
            valid_until: Utc::now() + chrono::Duration::seconds(60),
            nonce: 1,
        };
        assert_eq!(
            verify_beacon_signature(&peer, &payload, Utc::now(), None),
            BeaconVerifyOutcome::Malformed
        );
    }

    #[test]
    fn verify_expired() {
        let sig = pseudo_signature(&NodeId::new("node-A"));
        let peer = peer_with_sig(Some(&sig));
        let payload = BeaconPayload {
            node_id: NodeId::new("node-A"),
            endpoints: vec![],
            valid_until: Utc::now() - chrono::Duration::seconds(1),
            nonce: 1,
        };
        assert_eq!(
            verify_beacon_signature(&peer, &payload, Utc::now(), None),
            BeaconVerifyOutcome::Expired
        );
    }

    #[test]
    fn verify_node_id_mismatch() {
        let sig = pseudo_signature(&NodeId::new("node-A"));
        let peer = peer_with_sig(Some(&sig));
        let payload = BeaconPayload {
            node_id: NodeId::new("node-B"),
            endpoints: vec![],
            valid_until: Utc::now() + chrono::Duration::seconds(60),
            nonce: 1,
        };
        assert_eq!(
            verify_beacon_signature(&peer, &payload, Utc::now(), None),
            BeaconVerifyOutcome::NodeIdMismatch
        );
    }

    #[test]
    fn verify_ok_with_pseudo_sig_no_pubkey() {
        // 无公钥 → 骨架回退：伪签名结构合法即通过（保留旧行为）
        let sig = pseudo_signature(&NodeId::new("node-A"));
        let peer = peer_with_sig(Some(&sig));
        let payload = BeaconPayload {
            node_id: NodeId::new("node-A"),
            endpoints: vec![],
            valid_until: Utc::now() + chrono::Duration::seconds(60),
            nonce: 1,
        };
        assert_eq!(
            verify_beacon_signature(&peer, &payload, Utc::now(), None),
            BeaconVerifyOutcome::Ok
        );
    }

    #[test]
    fn pseudo_signature_length() {
        let s = pseudo_signature(&NodeId::new("x"));
        assert_eq!(s.len(), 128);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // —— hex 编解码测试 ——

    #[test]
    fn hex_encode_decode_roundtrip() {
        let bytes = [0x00, 0x01, 0xfe, 0xff, 0xab, 0xcd];
        let s = hex_encode(&bytes);
        assert_eq!(s, "0001feffabcd");
        assert_eq!(hex_decode(&s).unwrap(), bytes.to_vec());
    }

    #[test]
    fn hex_decode_rejects_garbage() {
        assert!(hex_decode("xyz").is_none()); // 奇数长度
        assert!(hex_decode("xy").is_none()); // 非 hex
        assert!(hex_decode("ab").is_some()); // ok
    }

    // —— 真实 ed25519 验签测试（本批新增）——

    /// 构造一个未过期的 payload（node_id 与 peer 对齐）
    fn fresh_payload(node_id: &NodeId, endpoints: Vec<String>, nonce: u64) -> BeaconPayload {
        BeaconPayload {
            node_id: node_id.clone(),
            endpoints,
            valid_until: Utc::now() + chrono::Duration::seconds(300),
            nonce,
        }
    }

    #[test]
    fn ed25519_sign_verify_roundtrip() {
        // 已知密钥对：签名 → 验签往返应通过
        let (sk, pk) = generate_keypair();
        let node_id = NodeId::new("node-real");
        let payload = fresh_payload(&node_id, vec!["10.0.0.5:8443".into()], 42);
        let sig_hex = sign_beacon(&sk, &payload);
        assert_eq!(sig_hex.len(), 128);

        let peer = PeerNode {
            node_id: node_id.clone(),
            endpoints: payload.endpoints.clone(),
            version: "1.5.0".into(),
            arch: "x86_64".into(),
            capabilities: NodeCapabilities::full(),
            beacon_signature: Some(sig_hex),
        };
        assert_eq!(
            verify_beacon_signature(&peer, &payload, Utc::now(), Some(&pk)),
            BeaconVerifyOutcome::Ok
        );
    }

    #[test]
    fn ed25519_rejects_wrong_key() {
        // 用 A 的私钥签名，用 B 的公钥验签 → BadSignature
        let (sk_a, _) = generate_keypair();
        let (_, pk_b) = generate_keypair();
        let node_id = NodeId::new("node-real");
        let payload = fresh_payload(&node_id, vec![], 1);
        let sig_hex = sign_beacon(&sk_a, &payload);
        let peer = PeerNode {
            node_id: node_id.clone(),
            endpoints: vec![],
            version: "1.5.0".into(),
            arch: "x86_64".into(),
            capabilities: NodeCapabilities::minimal(),
            beacon_signature: Some(sig_hex),
        };
        assert_eq!(
            verify_beacon_signature(&peer, &payload, Utc::now(), Some(&pk_b)),
            BeaconVerifyOutcome::BadSignature
        );
    }

    #[test]
    fn ed25519_rejects_tampered_payload() {
        // 签名后修改 payload 任一字段（这里改 nonce）→ BadSignature（防字段替换）
        let (sk, pk) = generate_keypair();
        let node_id = NodeId::new("node-real");
        let payload = fresh_payload(&node_id, vec!["e:1".into()], 1);
        let sig_hex = sign_beacon(&sk, &payload);
        let peer = PeerNode {
            node_id: node_id.clone(),
            endpoints: payload.endpoints.clone(),
            version: "1.5.0".into(),
            arch: "x86_64".into(),
            capabilities: NodeCapabilities::minimal(),
            beacon_signature: Some(sig_hex),
        };
        // 篡改 nonce
        let tampered = BeaconPayload {
            nonce: 999,
            ..payload.clone()
        };
        assert_eq!(
            verify_beacon_signature(&peer, &tampered, Utc::now(), Some(&pk)),
            BeaconVerifyOutcome::BadSignature
        );
    }

    #[test]
    fn ed25519_rejects_tampered_signature() {
        // 篡改签名一字节 → BadSignature（防签名伪造）
        let (sk, pk) = generate_keypair();
        let node_id = NodeId::new("node-real");
        let payload = fresh_payload(&node_id, vec![], 1);
        let mut sig_bytes = hex_decode(&sign_beacon(&sk, &payload)).unwrap();
        sig_bytes[0] ^= 0xff; // 翻转首字节
        let bad_sig_hex = hex_encode(&sig_bytes);
        let peer = PeerNode {
            node_id: node_id.clone(),
            endpoints: vec![],
            version: "1.5.0".into(),
            arch: "x86_64".into(),
            capabilities: NodeCapabilities::minimal(),
            beacon_signature: Some(bad_sig_hex),
        };
        assert_eq!(
            verify_beacon_signature(&peer, &payload, Utc::now(), Some(&pk)),
            BeaconVerifyOutcome::BadSignature
        );
    }

    #[test]
    fn ed25519_rejects_expired_beacon() {
        // 过期 beacon 在结构校验阶段就被拒绝（不进入密码学校验）
        let (sk, pk) = generate_keypair();
        let node_id = NodeId::new("node-real");
        let payload = BeaconPayload {
            node_id: node_id.clone(),
            endpoints: vec![],
            valid_until: Utc::now() - chrono::Duration::seconds(1),
            nonce: 1,
        };
        let sig_hex = sign_beacon(&sk, &payload);
        let peer = PeerNode {
            node_id: node_id.clone(),
            endpoints: vec![],
            version: "1.5.0".into(),
            arch: "x86_64".into(),
            capabilities: NodeCapabilities::minimal(),
            beacon_signature: Some(sig_hex),
        };
        assert_eq!(
            verify_beacon_signature(&peer, &payload, Utc::now(), Some(&pk)),
            BeaconVerifyOutcome::Expired
        );
    }

    #[test]
    fn ed25519_rejects_pseudo_signature_with_pubkey() {
        // 伪签名在真实验签路径下必被拒（防 mock 签名混入生产）
        let (_, pk) = generate_keypair();
        let node_id = NodeId::new("node-A");
        let sig = pseudo_signature(&node_id);
        let peer = peer_with_sig(Some(&sig));
        let payload = fresh_payload(&NodeId::new("node-A"), vec![], 1);
        assert_eq!(
            verify_beacon_signature(&peer, &payload, Utc::now(), Some(&pk)),
            BeaconVerifyOutcome::BadSignature
        );
    }

    #[test]
    fn osrng_nonce_is_random() {
        // OsRng nonce 两次生成不应相等（极小概率撞，可忽略）
        let a = generate_challenge_nonce();
        let b = generate_challenge_nonce();
        assert_ne!(a, b);
    }

    #[test]
    fn pubkey_fingerprint_is_sha256_hex() {
        let (_, pk) = generate_keypair();
        let fp = pubkey_fingerprint(&pk);
        assert_eq!(fp.len(), 64); // SHA-256 = 32 字节 = 64 hex
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
        // 不同公钥指纹不同
        let (_, pk2) = generate_keypair();
        assert_ne!(fp, pubkey_fingerprint(&pk2));
    }

    #[test]
    fn keypair_roundtrip_from_bytes() {
        // from_bytes 往返：同一私钥应产生同一公钥
        let (sk, pk) = generate_keypair();
        let sk2 = SigningKey::from_bytes(&sk.to_bytes());
        assert_eq!(sk2.verifying_key().to_bytes(), pk.to_bytes());
    }
}
