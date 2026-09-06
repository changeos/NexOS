//! 节点身份与 Overlay 地址——设计 §3「identity」（纯函数，无 I/O）。
//!
//! - [`NodeId`] = secp256k1 **压缩公钥**（`0x` + 66 hex）——与
//!   `os_common::chain_auth` 的链上身份完全同源（IM/NexHub 用户名即节点身份，
//!   身份体系零新增）。解析/验签直接委托 chain_auth。
//! - [`NodeIdentity`] = 私钥持有人（节点自身的完整身份），提供挑战签名
//!   （65 字节 `r||s||v`，与 chain_auth::verify_nonce_signature 逐字节兼容）。
//! - [`OverlayAddr`] = `keccak256(未压缩公钥[1..])[12..]` 的 **20 字节**——与
//!   EVM 地址派生同源（设计：Overlay 地址 = 哈希(节点公钥)，XOR 距离去中心化
//!   分片）。160-bit 地址空间 → 邻域阶（共同前导比特数）取值 0..=159，共
//!   160 个 proximity bin（Swarm 语义在 160-bit 地址上的同款投影）。
//!
//! # 距离与邻域阶（纯函数，可测）
//!
//! - [`OverlayAddr::xor`]：逐字节异或（20 字节）。
//! - [`OverlayAddr::proximity_order`]：共同前导比特数（`leading_zeros(xor)` 总和，
//!   相异公钥必 < 160；地址相等才可能是 160——路由表永不收录自身）。
//! - [`OverlayAddr::bucket_for`]：桶下标 = proximity order 本身（PO=p 的节点进
//!   第 p 桶；PO 越大越近，159 = 最近邻域）。

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Overlay 地址字节数（keccak256 输出 32 字节取 [12..]）。
pub const OVERLAY_LEN: usize = 20;
/// 邻域阶上限：160-bit 地址的最大共同前导比特数（仅当地址完全相等时取得；
/// 相异公钥的 OverlayAddr 必然不同 → 路由表内 PO 0..=159）。
pub const MAX_PROXIMITY_ORDER: u8 = 160;
/// 邻域桶数量 = MAX_PROXIMITY_ORDER（PO 0..=159 各一桶）。
pub const BUCKET_COUNT: usize = MAX_PROXIMITY_ORDER as usize;

// ============================================================================
// NodeId
// ============================================================================

/// 节点身份 = secp256k1 压缩公钥（33 字节；展示为 `0x` + 66 hex）。
///
/// 与 `os_common::chain_auth` 的链上身份同源：[`NodeId::parse`] 即 chain_auth 的
/// `parse_pubkey`（格式校验 + sec1 点校验）。`Hash`/`Eq` 按压缩字节，可作 HashMap
/// 键；serde 按十六进制字符串编码（协议帧内的 src/dst）。
#[derive(Clone, PartialEq, Eq)]
pub struct NodeId {
    /// 验签公钥（chain_auth 同栈，验签直接可用）。
    vk: k256::ecdsa::VerifyingKey,
    /// 压缩公钥字节（Hash/Eq/线上表示的判据）。
    compressed: [u8; 33],
}

impl NodeId {
    /// 从验签公钥构造。
    #[must_use]
    pub fn from_verifying_key(vk: &k256::ecdsa::VerifyingKey) -> Self {
        let compressed = compressed_bytes(vk);
        Self {
            vk: *vk,
            compressed,
        }
    }

    /// 解析 `0x` + 66 hex 压缩公钥（委托 chain_auth::parse_pubkey——非法格式、
    /// 非 sec1 点一律 None）。
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        os_common::chain_auth::parse_pubkey(s).map(|vk| {
            let compressed = compressed_bytes(&vk);
            Self { vk, compressed }
        })
    }

    /// 展示为 `0x` + 66 hex（与 chain_auth 身份字符串完全一致）。
    #[must_use]
    pub fn to_hex(&self) -> String {
        format!("0x{}", hex::encode(self.compressed))
    }

    /// 压缩公钥字节（33 字节）。
    #[must_use]
    pub fn compressed(&self) -> [u8; 33] {
        self.compressed
    }

    /// 验签公钥引用。
    #[must_use]
    pub fn verifying_key(&self) -> &k256::ecdsa::VerifyingKey {
        &self.vk
    }

    /// EVM 同源展示名（`0x` + 40 hex）——即 OverlayAddr 的 hex 形式，等价
    /// chain_auth::derive_display_name（跨节点对账人类可读地址）。
    #[must_use]
    pub fn display_name(&self) -> String {
        self.overlay().to_hex()
    }

    /// Overlay 地址（keccak256(未压缩公钥[1..])[12..]，EVM 同源 20 字节）。
    #[must_use]
    pub fn overlay(&self) -> OverlayAddr {
        OverlayAddr::from_verifying_key(&self.vk)
    }

    /// 验证 65 字节 `r||s||v` 挑战签名（直接复用 chain_auth——对端证明持有
    /// NodeID 私钥）。
    #[must_use]
    pub fn verify_signature(&self, nonce: &str, sig65: &[u8]) -> bool {
        os_common::chain_auth::verify_nonce_signature(&self.vk, nonce, sig65)
    }
}

fn compressed_bytes(vk: &k256::ecdsa::VerifyingKey) -> [u8; 33] {
    let point = vk.to_encoded_point(true);
    let bytes = point.as_bytes();
    let mut out = [0u8; 33];
    out.copy_from_slice(bytes);
    out
}

/// Hash 按压缩公钥字节（VerifyingKey 本身不实现 Hash）。
impl std::hash::Hash for NodeId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.compressed.hash(state);
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "NodeId({})", crate::short_hex(&self.to_hex()))
    }
}

impl std::str::FromStr for NodeId {
    type Err = P2pIdParseError;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Self::parse(s).ok_or(P2pIdParseError(s.to_string()))
    }
}

/// NodeId 字符串解析失败（格式非法或非 sec1 点）。
#[derive(Debug, thiserror::Error)]
#[error("invalid node id: {0}")]
pub struct P2pIdParseError(String);

impl Serialize for NodeId {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for NodeId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let hex = String::deserialize(d)?;
        NodeId::parse(&hex)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid node id: {hex}")))
    }
}

// ============================================================================
// NodeIdentity（私钥侧）
// ============================================================================

/// 节点完整身份（私钥持有人）。每个 P2pNode 持有一份；`generate` 用 CSPRNG。
///
/// 签名格式与 chain_auth 验签契约逐字节兼容：SHA-256(nonce) 摘要 → RFC6979
/// 可恢复 ECDSA → 65 字节 `r||s||v`。
#[derive(Clone)]
pub struct NodeIdentity {
    secret: k256::ecdsa::SigningKey,
}

impl NodeIdentity {
    /// CSPRNG 生成新身份（k256 OsRng，与 chain_auth 随机数同栈）。
    #[must_use]
    pub fn generate() -> Self {
        use k256::elliptic_curve::rand_core::{OsRng, RngCore};
        let mut b = [0u8; 32];
        OsRng.fill_bytes(&mut b);
        Self::from_seed(&b)
    }

    /// 从 32 字节私钥种子构造（测试复现同一身份：离线重连/信箱场景）。
    #[must_use]
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self {
            secret: k256::ecdsa::SigningKey::from_bytes(seed.into()).expect("32 字节私钥必可构造"),
        }
    }

    /// 从 SigningKey 构造。
    #[must_use]
    pub fn from_signing_key(sk: k256::ecdsa::SigningKey) -> Self {
        Self { secret: sk }
    }

    /// 本节点 NodeID（公钥侧）。
    #[must_use]
    pub fn node_id(&self) -> NodeId {
        NodeId::from_verifying_key(self.secret.verifying_key())
    }

    /// 对挑战 nonce 签名：65 字节 `r||s||v`（对端用 [`NodeId::verify_signature`]
    /// 即 chain_auth::verify_nonce_signature 验证）。
    #[must_use]
    pub fn sign(&self, nonce: &str) -> [u8; 65] {
        use sha2::Digest;
        let digest = sha2::Sha256::new_with_prefix(nonce.as_bytes());
        let (sig, recid) = self
            .secret
            .sign_digest_recoverable(digest)
            .expect("RFC6979 签名必成功");
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&sig.to_bytes());
        out[64] = u8::from(recid);
        out
    }

    /// 私钥字节数组（持久化/测试复现）。
    #[must_use]
    pub fn to_seed(&self) -> [u8; 32] {
        self.secret.to_bytes().into()
    }
}

impl fmt::Debug for NodeIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "NodeIdentity({})",
            crate::short_hex(&self.node_id().to_hex())
        )
    }
}

// ============================================================================
// OverlayAddr
// ============================================================================

/// Overlay 地址：keccak256(未压缩公钥[1..])[12..] 的 20 字节（EVM 地址同源）。
///
/// 逻辑拓扑坐标——Kademlia 的 XOR 距离与邻域阶都在 OverlayAddr 上定义；
/// 线上表示为 `0x` + 40 hex（FINDNODE 的 target）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OverlayAddr(pub [u8; OVERLAY_LEN]);

impl OverlayAddr {
    /// 从验签公钥派生（keccak256(未压缩[1..])[12..]，与 EVM 地址规则一致）。
    #[must_use]
    pub fn from_verifying_key(vk: &k256::ecdsa::VerifyingKey) -> Self {
        use tiny_keccak::{Hasher, Keccak};
        let uncompressed = vk.to_encoded_point(false); // 0x04 || X || Y（65 字节）
        let mut hasher = Keccak::v256();
        hasher.update(&uncompressed.as_bytes()[1..]);
        let mut hash = [0u8; 32];
        hasher.finalize(&mut hash);
        let mut out = [0u8; OVERLAY_LEN];
        out.copy_from_slice(&hash[12..]);
        Self(out)
    }

    /// 随机地址（桶刷新的 walk target；CSPRNG）。
    #[must_use]
    pub fn random() -> Self {
        use k256::elliptic_curve::rand_core::{OsRng, RngCore};
        let mut out = [0u8; OVERLAY_LEN];
        OsRng.fill_bytes(&mut out);
        Self(out)
    }

    /// `0x` + 40 hex。
    #[must_use]
    pub fn to_hex(&self) -> String {
        format!("0x{}", hex::encode(self.0))
    }

    /// 解析 `0x` + 40 hex。
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        let h = s.strip_prefix("0x")?;
        if h.len() != OVERLAY_LEN * 2 {
            return None;
        }
        let bytes = hex::decode(h).ok()?;
        let mut out = [0u8; OVERLAY_LEN];
        out.copy_from_slice(&bytes);
        Some(Self(out))
    }

    /// XOR 距离（逐字节；大端字节序字典序 == 数值序，可直接比较排序）。
    #[must_use]
    pub fn xor(&self, other: &Self) -> [u8; OVERLAY_LEN] {
        let mut out = [0u8; OVERLAY_LEN];
        for (o, (a, b)) in out.iter_mut().zip(self.0.iter().zip(other.0.iter())) {
            *o = a ^ b;
        }
        out
    }

    /// 邻域阶（proximity order）：两地址的**共同前导比特数**。
    ///
    /// - 首比特即不同 → 0（最远桶）；完全相等 → 160（仅自身，路由表不收录）。
    /// - 相异公钥的 OverlayAddr 几乎必然 PO < 160（碰撞概率 2^-160 量级）。
    #[must_use]
    pub fn proximity_order(&self, other: &Self) -> u8 {
        let d = self.xor(other);
        let mut bits = 0u32;
        for &b in &d {
            if b == 0 {
                bits += 8;
            } else {
                bits += b.leading_zeros();
                break;
            }
        }
        bits.min(u32::from(MAX_PROXIMITY_ORDER)) as u8
    }

    /// 该地址应落入的桶下标（== proximity order，0..=159）。
    /// PO=160（自身）映射到 159 不产生歧义——路由表在 upsert 前已排除自身。
    #[must_use]
    pub fn bucket_for(&self, other: &Self) -> usize {
        usize::from(self.proximity_order(other)).min(BUCKET_COUNT - 1)
    }
}

impl fmt::Display for OverlayAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for OverlayAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Overlay({})", crate::short_hex(&self.to_hex()))
    }
}

impl AsRef<[u8]> for OverlayAddr {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl Serialize for OverlayAddr {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for OverlayAddr {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let hex = String::deserialize(d)?;
        OverlayAddr::parse(&hex)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid overlay addr: {hex}")))
    }
}

// ============================================================================
// 单元测——纯函数：距离 / 邻域阶 / 桶选择 / NodeID 解析
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_id(seed: u8) -> (NodeIdentity, NodeId) {
        let id = NodeIdentity::from_seed(&[seed; 32]);
        (id.clone(), id.node_id())
    }

    // 1. NodeID 往返：generate → hex → parse 相等；overlay/display_name 一致
    #[test]
    fn node_id_roundtrip_and_stable_overlay() {
        let (_, id) = seed_id(7);
        let hex = id.to_hex();
        assert_eq!(hex.len(), 68, "0x + 66 hex");
        let parsed = NodeId::parse(&hex).expect("合法 hex 应可解析");
        assert_eq!(parsed, id);
        assert_eq!(id.overlay(), id.overlay(), "两次派生一致");
        assert_eq!(id.display_name(), id.overlay().to_hex());
        assert_eq!(id.display_name().len(), 42, "EVM 地址 0x + 40 hex");
    }

    // 2. EVM 同源公开向量：生成元公钥 → 0x7e5f…bdf（chain_auth 同款断言）
    #[test]
    fn overlay_matches_evm_known_vector() {
        let id =
            NodeId::parse("0x0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798")
                .expect("生成元公钥应可解析");
        assert_eq!(
            id.overlay().to_hex(),
            "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf"
        );
    }

    // 3. 邻域阶手工向量：首比特异 → 0；共同 1 比特 → 1；8+1 比特 → 9；
    //    前 19 字节全同 + 末字节 0xFF vs 0x80 → 152 + 1 = 159（最近桶）
    #[test]
    fn proximity_order_known_vectors() {
        let mk = |first: u8, second: u8| {
            let mut a = [0u8; 20];
            a[0] = first;
            a[1] = second;
            OverlayAddr(a)
        };
        // 0x80 = 1000_0000 vs 0x7F = 0111_1111：首比特不同 → PO 0
        assert_eq!(mk(0x80, 0).proximity_order(&mk(0x7F, 0)), 0);
        // 0x80 vs 0xC0 = 1100_0000：共同 1 比特 → PO 1
        assert_eq!(mk(0x80, 0).proximity_order(&mk(0xC0, 0)), 1);
        // 首字节相同（8 比特）+ 次字节 0x80 vs 0xC0 共同 1 比特 → PO 9
        assert_eq!(mk(0xAB, 0x80).proximity_order(&mk(0xAB, 0xC0)), 9);
        // 前 19 字节相同、末字节 0xFF vs 0xFE（仅末比特不同）→ 19*8 + 7 = 159
        let mut near1 = [0u8; 20];
        let mut near2 = [0u8; 20];
        near1[19] = 0xFF;
        near2[19] = 0xFE;
        assert_eq!(
            OverlayAddr(near1).proximity_order(&OverlayAddr(near2)),
            159,
            "最近邻域 PO=159"
        );
        // 自身 → 160（仅相等地址；路由表不收录）
        assert_eq!(OverlayAddr(near1).proximity_order(&OverlayAddr(near1)), 160);
    }

    // 4. XOR 距离 + 桶选择：xor 正确；bucket_for == PO（含 159 钳制）；随机键
    //    PO < 160（相异公钥）
    #[test]
    fn xor_distance_and_bucket_selection() {
        let a = OverlayAddr([
            0x0F, 0xF0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]);
        let b = OverlayAddr([
            0xF0, 0x0F, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]);
        assert_eq!(a.xor(&b)[0], 0xFF);
        assert_eq!(a.xor(&b)[1], 0xFF);
        assert!(a.xor(&b)[2..].iter().all(|&x| x == 0));
        assert_eq!(a.bucket_for(&b), 0);
        // PO=1 的对端进 1 号桶
        let c = OverlayAddr([
            0b1000_0000,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ]);
        let d = OverlayAddr([
            0b1100_0000,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ]);
        assert_eq!(c.bucket_for(&d), 1);
        // 相等地址（自身）钳制到最后一桶不越界
        assert_eq!(a.bucket_for(&a), BUCKET_COUNT - 1);
        // 随机身份两两 PO < 160 且 ≥ 0
        let (_, i1) = seed_id(1);
        let (_, i2) = seed_id(2);
        let po = i1.overlay().proximity_order(&i2.overlay());
        assert!(po < 160, "相异公钥 PO < 160，实测 {po}");
    }

    // 5. NodeID 解析拒绝非法（与 chain_auth 契约一致），serde 编解码往返
    #[test]
    fn node_id_parse_rejects_invalid_and_serde_roundtrip() {
        let (_, id) = seed_id(3);
        let valid = id.to_hex();
        assert!(NodeId::parse(&valid).is_some());
        for bad in [
            valid[2..].to_string(),           // 缺 0x
            format!("0x{}", &valid[2..66]),   // 长度不足
            format!("0x{}zz", &valid[2..64]), // 非 hex
            String::new(),
        ] {
            assert!(NodeId::parse(&bad).is_none(), "应拒绝: {bad}");
        }
        // serde：帧内以 hex 字符串出现
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, format!("\"{valid}\""));
        let back: NodeId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
        assert!(serde_json::from_str::<NodeId>("\"0x00\"").is_err());
        // OverlayAddr serde 往返
        let ov = id.overlay();
        let back: OverlayAddr = serde_json::from_str(&serde_json::to_string(&ov).unwrap()).unwrap();
        assert_eq!(back, ov);
    }

    // 6. 挑战签名契约：sign 产物被 NodeId::verify_signature（chain_auth）验证；
    //    跨密钥签名拒绝
    #[test]
    fn sign_verify_challenge_contract() {
        let (alice, alice_id) = seed_id(0xA1);
        let (bob, bob_id) = seed_id(0xB2);
        let nonce = "deadbeef".repeat(8);
        let sig = alice.sign(&nonce);
        assert_eq!(sig.len(), 65);
        assert!(alice_id.verify_signature(&nonce, &sig), "本尊签名通过");
        assert!(!bob_id.verify_signature(&nonce, &sig), "他人签名拒绝");
        assert!(
            !alice_id.verify_signature(&nonce, &bob.sign(&nonce)),
            "Bob 的签名不属于 Alice"
        );
        assert!(
            bob_id.verify_signature(&nonce, &bob.sign(&nonce)),
            "Bob 本尊签名在 Bob 公钥下应通过（对照组）"
        );
        assert!(
            !alice_id.verify_signature(&nonce, &sig[..64]),
            "非 65 字节拒绝"
        );
    }

    // 7. 同种子复现同身份（离线重连/信箱测试的前提）；不同种子不同身份
    #[test]
    fn identity_reproducible_from_seed() {
        let a = NodeIdentity::from_seed(&[9; 32]);
        let b = NodeIdentity::from_seed(&[9; 32]);
        let c = NodeIdentity::from_seed(&[8; 32]);
        assert_eq!(a.node_id(), b.node_id());
        assert_eq!(a.to_seed(), [9; 32]);
        assert_ne!(a.node_id(), c.node_id());
        assert_ne!(
            NodeIdentity::generate().node_id(),
            NodeIdentity::generate().node_id(),
            "CSPRNG 生成不重复"
        );
    }

    // 8. OverlayAddr::random 均匀散布 + parse hex 往返
    #[test]
    fn overlay_random_and_parse() {
        let a = OverlayAddr::random();
        let b = OverlayAddr::random();
        assert_ne!(a, b);
        assert_eq!(OverlayAddr::parse(&a.to_hex()), Some(a));
        assert_eq!(OverlayAddr::parse("0x1234"), None, "长度不足");
        assert_eq!(OverlayAddr::parse("1234..."), None, "缺 0x");
    }
}
